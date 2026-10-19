use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    },
    /// What happened after a cursor: trunk landings, log events, new invalidations of the
    /// view's change
    Watch {
        /// The cursor a previous watch returned (`detail.cursor`)
        #[arg(long, value_name = "CURSOR", value_parser = bay::watch::Cursor::parse)]
        since: Option<bay::watch::Cursor>,
    },
    /// Admin: probe this box (git and Claude Code floors, remote, hook entries, installed skill)
    Doctor,
    /// Admin: place bay at $HOME/.local/bin/bay, its skill and its project hook entries
    Install {
        /// Only the box's part: the binary and the skill, never a repository's settings or Git config
        #[arg(long)]
        machine: bool,
    },
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
    /// Admin: replace this binary with a verified release (the box's pin wins)
    Update {
        /// The release channel (default: git config --global bay.updateChannel, else stable)
        #[arg(long, value_parser = ["stable", "latest"])]
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Start {
            intent,
            intent_revision,
            paths,
            inputs,
            resume,
            new,
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
        Command::Live {
            action: LiveAction::Push,
        } => bay::live::push(&cli.dir),
        Command::Status { fleet: true, .. } => bay::fleet::fleet(&cli.dir),
        Command::Status { change, .. } => {
            let result = bay::status::status(&cli.dir, change.as_deref());
            bay::update::auto();
            result
        }
        Command::Watch { since } => bay::watch::watch(&cli.dir, since.as_ref()),
        Command::Doctor => {
            let result = bay::doctor::doctor(&cli.dir);
            bay::update::auto();
            result
        }
        Command::Install { machine } => bay::install::install(&cli.dir, machine),
        Command::Update {
            channel,
            version,
            check,
        } => bay::update::update(&bay::update::Request {
            channel,
            version,
            check,
        }),
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
