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
    /// or, named by its id, pick up an existing change, as --resume does
    Start {
        /// The intent (a card, issue or ticket id), or an existing change's id to pick it up
        #[arg(required_unless_present = "resume")]
        intent: Option<String>,
        /// The revision of the intent's external reference
        #[arg(long, default_value = "1", hide = true)]
        intent_revision: String,
        /// The paths this change reads and writes (its declared inputs)
        #[arg(long, num_args = 1..)]
        paths: Vec<String>,
        /// A named input this change depends on, `<kind>:<name>` (repeatable)
        #[arg(long = "input", value_name = "KIND:NAME", value_parser = bay::inputs::InputName::parse)]
        inputs: Vec<bay::inputs::InputName>,
        /// Restore an existing change: checkpoint its draft, bring in newer main (the same as
        /// `bay start <CHANGE>`)
        #[arg(long, value_name = "CHANGE", conflicts_with = "intent")]
        resume: Option<String>,
        /// No longer needed after a landing: plain start takes the intent's next generation.
        /// After an abandon, claim the next generation instead of adopting the abandoned change
        #[arg(long)]
        new: bool,
        /// The landed change this one fixes; its landing carries a Fixes: trailer
        #[arg(long, value_name = "CHANGE", conflicts_with = "resume")]
        fixes: Option<String>,
        /// A sandbox lane of the config: the change's checks run, and it never lands
        #[arg(long, value_name = "LANE", conflicts_with = "resume")]
        lane: Option<String>,
        /// A branch made outside bay (a branch name or a full ref): its net change is the draft
        #[arg(long, value_name = "REF", conflicts_with = "resume")]
        from: Option<String>,
        /// A change this one lands only after (repeatable): it publishes once that change landed
        #[arg(long, value_name = "CHANGE", conflicts_with = "resume")]
        after: Vec<String>,
    },
    /// Freeze the draft, build the candidate, check it, publish it
    Land {
        /// The change (default: the change of the view bay runs in)
        change: Option<String>,
        /// The change, as a flag: the same as the positional one, kept for older scripts
        #[arg(
            long = "change",
            value_name = "CHANGE",
            hide = true,
            conflicts_with = "change"
        )]
        change_flag: Option<String>,
        #[command(flatten)]
        wait: WaitArgs,
        /// Submit the abandon request instead: the change is given up, any actor may adopt it
        #[arg(long, conflicts_with_all = ["wait", "observe"])]
        abandon: bool,
    },
    /// Give a change up: the abandon request `bay land --abandon` submits; any actor may adopt it
    Abandon {
        /// The change (default: the change of the view bay runs in)
        change: Option<String>,
    },
    /// Land the edits made in the main checkout as one change: start, land --wait, then move
    /// the checkout to the new tip of main. Never in a view (use land there)
    Save {
        /// The intent: a card, issue or ticket id
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
        /// Say no instead: the record's verdict is `product_failure`, and the change is blocked
        #[arg(long, conflicts_with = "waive")]
        reject: bool,
        /// Waive this one check, marked `waivable = true`, on the pending candidate: a signed
        /// waiver, never a pass; the landing names it as a coverage gap. Needs --why
        #[arg(long, value_name = "CHECK")]
        waive: Option<String>,
        /// Why, in the approver's words: kept in the signed record
        #[arg(long, value_name = "TEXT")]
        why: Option<String>,
    },
    /// A new change that undoes one landing, checked and landed like any other; never a rewrite
    Undo {
        /// The landed change to undo
        change: String,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Current state, derived on read from main and refs
    Status {
        /// One change (default: the view's change, else every local change)
        change: Option<String>,
        /// The fleet map from the local cache `bay status` fetches; fails closed without that fetch
        #[arg(long, conflicts_with = "change")]
        fleet: bool,
        /// Local branches no remote-tracking ref holds, and worktree records whose folder is
        /// gone; local refs only, never a fetch, a prune or a delete
        #[arg(long, conflicts_with_all = ["change", "fleet"])]
        stranded: bool,
        /// The Reverts:, Supersedes: and Fixes: pairs of a first-parent range (default: main);
        /// local refs only, never a fetch
        #[arg(long, value_name = "RANGE", num_args = 0..=1, default_missing_value = "",
            conflicts_with_all = ["change", "fleet", "stranded"])]
        lineage: Option<String>,
        /// Every branch on the remote, classed trunk, target, lane, landed (with its proof),
        /// stale or unique, and the stranded heads of --stranded; one ls-remote and one fetch,
        /// nothing deleted
        #[arg(long, conflicts_with_all = ["change", "fleet", "stranded", "lineage"])]
        heads: bool,
        /// With --heads: the same as `bay prune`, kept for older scripts
        #[arg(long, requires = "heads")]
        prune: bool,
        /// With --heads: a branch with commits off main whose tip is older than this is stale
        /// (default 30)
        #[arg(long, value_name = "DAYS", requires = "heads")]
        stale_days: Option<u64>,
    },
    /// Delete the remote's branches landed by ancestor or patch, each under a lease of the tip
    /// judged, after archiving the tip on the remote and logging the push that restores it to
    /// bay/pruned-heads.log in the repository's common directory; then runs the config's
    /// `[heads] on_prune` adapter, when set, once per deleted branch
    Prune {
        /// A branch with commits off main whose tip is older than this is stale, and kept
        /// (default 30)
        #[arg(long, value_name = "DAYS")]
        stale_days: Option<u64>,
    },
    /// What happened after a cursor: landings on main, log events, new invalidations of the
    /// view's change
    Watch {
        /// The cursor a previous watch returned (`detail.cursor`)
        #[arg(long, value_name = "CURSOR", value_parser = bay::watch::Cursor::parse)]
        since: Option<bay::watch::Cursor>,
        /// Only the landings: each with its base, committer time and whether it landed outside
        /// bay, and the count of commits off the first-parent line
        #[arg(long)]
        landed: bool,
    },
    /// Send one talk to one agent: one action, one target, the time the reply is owed. Arrival is
    /// the recipient's ack, written when its bay fetches
    Talk(Box<TalkArgs>),
    /// Admin: add one key to the key registry: append it to its class file, sign the new
    /// registry with this machine's registry key (`bay.registryKey`) and push it. The first
    /// key enrolled is that registry key
    Enrol(EnrolArgs),
    /// Admin: probe this box (git and Claude Code floors, remote, hook entries, installed skill)
    Doctor {
        /// Judge this box in this role instead of the one doctor infers
        #[arg(long, value_parser = ["executor"])]
        role: Option<String>,
    },
    /// Admin: place bay at $HOME/.local/bin/bay, its skill and its project hook entries
    Install(InstallArgs),
    /// Admin: a bay-hosted main
    Host {
        #[command(subcommand)]
        action: HostAction,
    },
    /// Admin: the executor that finishes operations after their agents leave
    Executor {
        #[command(subcommand)]
        action: ExecutorAction,
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
    /// Admin: replace this binary with a verified release (the box's pin wins)
    Update {
        /// Accepted and ignored, so older scripts keep working: every release is a candidate
        #[arg(long, hide = true)]
        channel: Option<String>,
        /// Move to this exact version (X.Y.Z or X.Y.Z-suffix)
        #[arg(long)]
        version: Option<String>,
        /// Report the installed version, the target and whether an update is due; change nothing
        #[arg(long)]
        check: bool,
    },
    /// Admin: prove a connector keeps an adapter contract, run as bay runs it; changes nothing
    Adapter {
        #[command(subcommand)]
        action: AdapterAction,
    },
    /// Admin: the agent hook entry point (`bay hook <event>`)
    Hook {
        /// The agent product's event, for example `pre-tool`
        event: String,
    },
    /// Every command and flag on one page, grouped by who runs it; or the help of one command.
    /// With --json: the same list for programs, every reason code, and the JSON Schema of
    /// bay.result/1
    Help {
        /// The command, for example `host init` (default: bay itself)
        #[arg(value_name = "COMMAND")]
        command: Vec<String>,
    },
}

/// `bay enrol <class> <name> <public-key-file>`.
#[derive(Args)]
struct EnrolArgs {
    /// The key's class
    #[arg(value_parser = ["agent", "executor", "producer", "reviewer", "approver", "registry"])]
    class: String,
    /// The name the key is enrolled under
    name: String,
    /// The public key file (`<key>.pub`)
    #[arg(value_name = "PUBLIC_KEY_FILE")]
    public_key: PathBuf,
}

/// `--wait[=SECONDS]`, the one flag `bay land` and `bay undo` share. `Option<Option<_>>` is
/// clap's flag with an optional value: absent, present alone, or present with SECONDS.
#[derive(Args)]
#[allow(clippy::option_option)]
struct WaitArgs {
    /// Run the publication here and return its outcome, instead of returning at once. With
    /// SECONDS: watch it that long, then return while the run goes on
    #[arg(long, value_name = "SECONDS", num_args = 0..=1, require_equals = true)]
    wait: Option<Option<u64>>,
    /// The same as --wait=SECONDS, kept for older scripts
    #[arg(long, value_name = "SECONDS", hide = true)]
    observe: Option<u64>,
}

impl WaitArgs {
    /// Whether to wait, and for how long at most: `None` waits for the outcome.
    fn wait(&self) -> (bool, Option<std::time::Duration>) {
        let seconds = self.wait.flatten().or(self.observe);
        (
            self.wait.is_some() || self.observe.is_some(),
            seconds.map(std::time::Duration::from_secs),
        )
    }
}

#[derive(Args)]
struct InstallArgs {
    /// Only the box's part: the binary and the skill, never a repository's settings or Git config
    #[arg(long)]
    machine: bool,
    /// The repository's main branch, written to git config bay.trunk (default: what it holds,
    /// else the remote's default branch)
    #[arg(long, conflicts_with = "machine")]
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
    /// Send to the agent that landed this change (a change id or its commit on main), read from
    /// bay's publication record; then the one argument is the action
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
    /// Create a bare repository on reftable whose main is append-only
    Init {
        path: PathBuf,
        #[arg(long, default_value = "main")]
        trunk: String,
        /// By default the object format of the repository bay runs in, else Git's default
        #[arg(long, value_parser = ["sha1", "sha256"])]
        object_format: Option<String>,
    },
    /// The host hook Git runs on a push (configured by `host init`)
    #[command(hide = true)]
    Hook { event: String, args: Vec<String> },
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
    /// Trunk's first-parent commits with no record of the kind from the producer about that
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
    /// Write a signed record about landings on main, as an outside producer or reviewer, or about
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
        /// A landing the record is about (for an approval, the candidate commit the land names);
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
enum AdapterAction {
    /// Run the contract cases of one adapter kind against a command; exits 0 only when every
    /// case passes
    Test(AdapterTestArgs),
}

#[derive(Args)]
struct AdapterTestArgs {
    /// The adapter kind: `check` (a policy's `[[check]]`), `wake` (`[wake] command`) or
    /// `notify` (`[notify] command`)
    #[arg(value_parser = bay::adapter::KINDS)]
    kind: String,
    /// Check: the entry's `timeout_s` (default: the policy's default)
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
    /// Check: another adapter path the command reads, relative to the repository (repeatable)
    #[arg(long = "with", value_name = "PATH")]
    with: Vec<String>,
    /// The adapter's command, as the config or policy names it: a program path relative to
    /// the repository, then its arguments
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum ExecutorAction {
    /// Drive queued operations to an outcome on this machine
    Run {
        /// Make one pass and exit
        #[arg(long)]
        once: bool,
        /// Seconds between passes
        #[arg(long, default_value = "10")]
        interval_s: u64,
    },
    /// Write a service that keeps `bay executor run` running for this repository (a launchd
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
/// `.bay/config.toml` committed at trunk (ruling: config location). The host, doctor's box
/// rows, install, update and the agent hook do not.
fn config_refusal(cli: &Cli) -> Option<BayResult> {
    // `status --fleet` never fetches (G49): it reads the config without a fetch itself.
    // `status --stranded` reads local refs only, and needs no config (G156).
    let needs_config = !matches!(
        cli.command,
        Command::Status { fleet: true, .. }
            | Command::Status { stranded: true, .. }
            | Command::Host { .. }
            | Command::Doctor { .. }
            | Command::Install(_)
            | Command::Update { .. }
            | Command::Hook { .. }
            | Command::Adapter { .. }
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
    if !matches!(cli.command, Command::Executor { .. }) {
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

/// `bay executor run`: its result, then, between operations, the new version in its place.
fn executor_run(dir: &std::path::Path, json: bool, once: bool, interval_s: u64) -> ExitCode {
    let result = bay::executor::run(dir, once, std::time::Duration::from_secs(interval_s));
    bay::coord::after_command();
    let code = render(&result, json);
    // Returns only when the executor does not move, or cannot.
    bay::executor::reexec(&result);
    code
}

/// `bay approve`: the approval its arguments ask for.
fn approve_change(dir: &std::path::Path, command: Command) -> BayResult {
    let Command::Approve {
        change,
        reject,
        waive,
        why,
    } = command
    else {
        unreachable!("approve_change takes an approve command");
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

/// `bay abandon [CHANGE]` (G301): the abandon request `bay land --abandon` submits, by the same
/// call; with no change, the change of the view bay runs in.
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
        },
    )
}

/// `bay start`: the request its arguments make.
fn start_change(dir: &std::path::Path, command: Command) -> BayResult {
    let Command::Start {
        intent,
        intent_revision,
        paths,
        inputs,
        resume,
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
            intent,
            new,
            intent_revision,
            paths,
            inputs,
            resume,
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
        approve @ Command::Approve { .. } => approve_change(&cli.dir, approve),
        Command::Undo { change, wait } => {
            let (wait, observe) = wait.wait();
            bay::undo::undo(&cli.dir, &change, wait, observe)
        }
        Command::Share {
            change,
            bundle,
            remote,
        } => bay::share::share(&cli.dir, &change, bundle.as_deref(), remote.as_deref()),
        Command::Land {
            change,
            change_flag,
            wait,
            abandon,
        } => {
            let (wait, observe) = wait.wait();
            bay::publish::land(
                &cli.dir,
                change.or(change_flag).as_deref(),
                wait,
                observe,
                abandon,
            )
        }
        Command::Abandon { change } => abandon(&cli.dir, change.as_deref()),
        Command::Host {
            action:
                HostAction::Init {
                    path,
                    trunk,
                    object_format,
                },
        } => bay::host::init(&cli.dir, &path, &trunk, object_format.as_deref()),
        Command::Host {
            action: HostAction::Hook { event, args },
        } => return ExitCode::from(bay::host::hook(&event, &args)),
        Command::Executor {
            action: ExecutorAction::Run { once, interval_s },
        } => return executor_run(&cli.dir, cli.json, once, interval_s),
        Command::Executor {
            action: ExecutorAction::Install { print, force },
        } => bay::service::install(&cli.dir, &bay::service::Request { print, force }),
        Command::Live { action } => live(&cli.dir, action),
        command @ Command::Status { .. } => status(&cli.dir, command),
        // The one prune: the same request `status --heads --prune` makes (G302).
        Command::Prune { stale_days } => heads(&cli.dir, true, stale_days),
        Command::Watch { since, landed } if landed => bay::watch::landed(&cli.dir, since.as_ref()),
        Command::Watch { since, .. } => bay::watch::watch(&cli.dir, since.as_ref()),
        Command::Talk(args) => talk(&cli.dir, *args),
        Command::Evidence { action } => evidence(&cli.dir, action),
        Command::Enrol(args) => enrol(&cli.dir, args),
        Command::Doctor { role } => doctor(&cli.dir, role.as_deref()),
        Command::Install(args) => install(&cli.dir, args),
        Command::Update {
            channel: _,
            version,
            check,
        } => bay::update::update(&bay::update::Request { version, check }),
        Command::Adapter { action } => adapter(&cli.dir, action),
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
    // A command that fetched trunk leaves the coordination cache the `pre-tool` hook reads
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
    let result = bay::doctor::doctor_as(dir, role == Some("executor"));
    bay::update::auto();
    result
}

/// `bay adapter test <kind> -- <command...>`: the conformance kit, which changes nothing.
fn adapter(dir: &std::path::Path, action: AdapterAction) -> BayResult {
    let AdapterAction::Test(args) = action;
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
/// path of `bay status --heads`, `bay prune` and `bay status --heads --prune` (G302).
fn heads(dir: &std::path::Path, prune: bool, stale_days: Option<u64>) -> BayResult {
    bay::heads::heads(dir, &bay::heads::Request { prune, stale_days })
}

/// `bay status` and its read-only views: the fleet map, stranded work, lineage pairs, every
/// head, or one change's state (which alone may start the daily update check).
fn status(dir: &std::path::Path, command: Command) -> BayResult {
    let Command::Status {
        change,
        fleet,
        stranded,
        lineage,
        heads: every_head,
        prune,
        stale_days,
    } = command
    else {
        unreachable!("status takes a status command");
    };
    if every_head {
        return heads(dir, prune, stale_days);
    }
    if fleet {
        return bay::fleet::fleet(dir);
    }
    if stranded {
        return bay::stranded::stranded(dir);
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
