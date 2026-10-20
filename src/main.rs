use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use bay::result::BayResult;

#[derive(Parser)]
#[command(version = bay::version::BAY, about)]
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
    /// Open a change: a fresh trunk, a sparse view of the declared paths, the change record
    Start {
        /// The intent: a card, issue or ticket id
        #[arg(required_unless_present = "resume")]
        intent: Option<String>,
        /// The revision of the intent's external reference
        #[arg(long, default_value = "1")]
        intent_revision: String,
        /// The paths this change reads and writes (its declared inputs)
        #[arg(long, num_args = 1..)]
        paths: Vec<String>,
        /// A named input this change depends on, `<kind>:<name>` (repeatable)
        #[arg(long = "input", value_name = "KIND:NAME", value_parser = bay::inputs::InputName::parse)]
        inputs: Vec<bay::inputs::InputName>,
        /// Restore an existing change: checkpoint its draft, bring in newer trunk
        #[arg(long, value_name = "CHANGE", conflicts_with = "intent")]
        resume: Option<String>,
        /// A follow-up after the intent's change landed: claim its next generation
        #[arg(long)]
        new: bool,
        /// The landed change this one fixes; its landing carries a Fixes: trailer
        #[arg(long, value_name = "CHANGE", conflicts_with = "resume")]
        fixes: Option<String>,
    },
    /// Freeze the draft, build the candidate, check it, publish it
    Land {
        /// The change (default: the change of the view bay runs in)
        #[arg(long)]
        change: Option<String>,
        /// Run the publication here and return its outcome, instead of returning at once
        #[arg(long)]
        wait: bool,
        /// With --wait: stop watching after this many seconds; the run continues
        #[arg(long, value_name = "SECONDS", requires = "wait")]
        observe: Option<u64>,
        /// Submit the abandon request instead: the change is given up, any actor may adopt it
        #[arg(long, conflicts_with_all = ["wait", "observe"])]
        abandon: bool,
    },
    /// A new change that undoes one landing, checked and landed like any other; never a rewrite
    Undo {
        /// The landed change to undo
        change: String,
        /// Run the publication here and return its outcome, instead of returning at once
        #[arg(long)]
        wait: bool,
    },
    /// Current state, derived on read from trunk and refs
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
        /// The Reverts:, Supersedes: and Fixes: pairs of a first-parent range (default: trunk);
        /// local refs only, never a fetch
        #[arg(long, value_name = "RANGE", num_args = 0..=1, default_missing_value = "",
            conflicts_with_all = ["change", "fleet", "stranded"])]
        lineage: Option<String>,
    },
    /// What happened after a cursor: trunk landings, log events, new invalidations of the
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
    /// Admin: probe this box (git and Claude Code floors, remote, hook entries, installed skill)
    Doctor {
        /// Judge this box in this role instead of the one doctor infers
        #[arg(long, value_parser = ["executor"])]
        role: Option<String>,
    },
    /// Admin: place bay at $HOME/.local/bin/bay, its skill and its project hook entries
    Install(InstallArgs),
    /// Admin: a bay-hosted trunk
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
    /// Admin: the agent hook entry point (`bay hook <event>`)
    Hook {
        /// The agent product's event, for example `pre-tool`
        event: String,
    },
}

#[derive(Args)]
struct InstallArgs {
    /// Only the box's part: the binary and the skill, never a repository's settings or Git config
    #[arg(long)]
    machine: bool,
    /// The repository's trunk branch, written to git config bay.trunk (default: what it holds,
    /// else the remote's default branch)
    #[arg(long, conflicts_with = "machine")]
    trunk: Option<String>,
    /// The trunk's remote, written to git config bay.remote (default: what it holds, else
    /// origin)
    #[arg(long, conflicts_with = "machine")]
    remote: Option<String>,
}

#[derive(Args)]
struct TalkArgs {
    /// The agent who owes the reply (a person's name works too)
    #[arg(value_name = "AGENT")]
    actor: Option<String>,
    /// Send to the agent that landed this change (a change id or its trunk commit), read from
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
    /// Create a bare repository on reftable whose trunk is append-only
    Init {
        path: PathBuf,
        #[arg(long, default_value = "main")]
        trunk: String,
        #[arg(long, default_value = "sha1", value_parser = ["sha1", "sha256"])]
        object_format: String,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(refusal) = config_refusal(&cli) {
        return render(&refusal, cli.json);
    }
    let result = match cli.command {
        Command::Start {
            intent,
            intent_revision,
            paths,
            inputs,
            resume,
            new,
            fixes,
        } => bay::start::start(
            &cli.dir,
            &bay::start::StartRequest {
                intent,
                new,
                intent_revision,
                paths,
                inputs,
                resume,
                reverts: None,
                fixes,
            },
        ),
        Command::Undo { change, wait } => bay::undo::undo(&cli.dir, &change, wait),
        Command::Share {
            change,
            bundle,
            remote,
        } => bay::share::share(&cli.dir, &change, bundle.as_deref(), remote.as_deref()),
        Command::Land {
            change,
            wait,
            observe,
            abandon,
        } => bay::publish::land(
            &cli.dir,
            change.as_deref(),
            wait,
            observe.map(std::time::Duration::from_secs),
            abandon,
        ),
        Command::Host {
            action:
                HostAction::Init {
                    path,
                    trunk,
                    object_format,
                },
        } => bay::host::init(&path, &trunk, &object_format),
        Command::Host {
            action: HostAction::Hook { event, args },
        } => return ExitCode::from(bay::host::hook(&event, &args)),
        Command::Executor {
            action: ExecutorAction::Run { once, interval_s },
        } => bay::executor::run(&cli.dir, once, std::time::Duration::from_secs(interval_s)),
        Command::Live { action } => live(&cli.dir, action),
        Command::Status {
            change,
            fleet,
            stranded,
            lineage,
        } => status(
            &cli.dir,
            change.as_deref(),
            fleet,
            stranded,
            lineage.as_deref(),
        ),
        Command::Watch { since, landed } if landed => bay::watch::landed(&cli.dir, since.as_ref()),
        Command::Watch { since, .. } => bay::watch::watch(&cli.dir, since.as_ref()),
        Command::Talk(args) => talk(&cli.dir, *args),
        Command::Evidence { action } => evidence(&cli.dir, action),
        Command::Doctor { role } => {
            let result = bay::doctor::doctor_as(&cli.dir, role.as_deref() == Some("executor"));
            bay::update::auto();
            result
        }
        Command::Install(args) => install(&cli.dir, args),
        Command::Update {
            channel: _,
            version,
            check,
        } => bay::update::update(&bay::update::Request { version, check }),
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

/// `bay evidence <action>`.
/// The hidden `bay live` commands a hook starts: push the live summary, or checkpoint a draft.
fn live(dir: &std::path::Path, action: LiveAction) -> BayResult {
    match action {
        LiveAction::Push => bay::live::push(dir),
        LiveAction::Checkpoint { change } => bay::start::checkpoint(dir, &change),
    }
}

/// `bay status` and its read-only views: the fleet map, stranded work, lineage pairs, or one
/// change's state (which alone may start the daily update check).
fn status(
    dir: &std::path::Path,
    change: Option<&str>,
    fleet: bool,
    stranded: bool,
    lineage: Option<&str>,
) -> BayResult {
    if fleet {
        return bay::fleet::fleet(dir);
    }
    if stranded {
        return bay::stranded::stranded(dir);
    }
    if let Some(range) = lineage {
        return bay::lineage::lineage(dir, Some(range));
    }
    let result = bay::status::status(dir, change);
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
    // Stdout is always the result document; human text goes to stderr (`INTERFACES` § 1).
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
