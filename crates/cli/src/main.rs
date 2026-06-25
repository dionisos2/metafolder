//! `mf` binary: argument parsing (clap) and dispatch to
//! [`metafolder_cli::commands`]. Exit codes (spec-main): 0 success,
//! 1 operation failed, 2 usage error (clap also exits 2 on bad arguments).
//!
//! The command tree mirrors the data model (spec-data-model "* CLI"): a verb
//! quartet (`get`/`set`/`add`/`delete`) at the metarecord, field-name and
//! field-id levels, plus the `repo`/`task`/`log` management groups. Repository
//! and daemon are selected once by the global `-n`/`-u`/`-p` flags.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use metafolder_cli::commands::{self, Ctx};
use metafolder_cli::gui::{self, GuiCtx};
use metafolder_cli::log;

#[derive(Parser)]
#[command(name = "mf", about = "metafolder CLI — thin client over the daemon HTTP API")]
struct Cli {
    /// Target repository by (unique) name
    #[arg(short = 'n', long = "name", global = true, env = "METAFOLDER_REPO_NAME")]
    repo_name: Option<String>,

    /// Target repository by UUID
    #[arg(short = 'u', long = "uuid", global = true, env = "METAFOLDER_REPO")]
    repo_uuid: Option<String>,

    /// Daemon port on 127.0.0.1
    #[arg(short = 'p', long = "port", global = true, env = "METAFOLDER_DAEMON_PORT", default_value_t = 7523)]
    port: u16,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Repository management (default: list)
    Repo {
        #[command(subcommand)]
        command: Option<RepoCommand>,
    },
    /// Background tasks (default: list)
    Task {
        #[command(subcommand)]
        command: Option<TaskCommand>,
    },
    /// Revision/operation history (default: list)
    Log {
        #[command(subcommand)]
        command: Option<LogCommand>,
    },
    /// Metarecord operations (default: get)
    Metarecord {
        #[command(subcommand)]
        command: Option<MetarecordCommand>,
    },
    /// Field operations by row id (direct access)
    Field {
        #[command(subcommand)]
        command: FieldCommand,
    },
    /// Convert a field's value type repository-wide (string|int|float|bool|datetime)
    Retype {
        /// Field name
        name: String,
        /// Target type: string, int, float, bool, or datetime
        to: String,
    },
    /// Reconcile the database with the filesystem
    Reconcile {
        /// Single-metarecord reconcile, scoped to this metarecord's subtree
        #[arg(long)]
        metarecord: Option<String>,
        /// Enable the similarity phase with this minimum score, range [0, 1]
        #[arg(long)]
        threshold: Option<f64>,
        /// Do not compute mfr_mime for files that lack it
        #[arg(long = "no-mime")]
        no_mime: bool,
        /// Do not refresh mfr_* stat fields of in-place (unmoved) files
        #[arg(long = "no-refresh")]
        no_refresh: bool,
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
        /// Start the (full) reconcile and print its task id without waiting
        #[arg(long = "no-wait")]
        no_wait: bool,
        /// Poll interval in milliseconds while waiting for the task
        #[arg(long = "poll-interval", default_value_t = 200)]
        poll_interval: u64,
    },
    /// Create the metarecord for a single path and print its UUID
    Track { path: PathBuf },
    /// Print the filesystem path of a metarecord (walks the mfr_path chain)
    Path {
        uuid: String,
        /// Print the path relative to the repository root
        #[arg(long)]
        relative: bool,
    },
    /// User schema commands
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Drive the GUI through its scripting HTTP API
    Gui {
        /// GUI base URL (default: gui-port from the GUI config.toml)
        #[arg(long, env = "METAFOLDER_GUI_URL")]
        gui_url: Option<String>,
        #[command(subcommand)]
        command: GuiCommand,
    },
}

#[derive(Subcommand)]
enum RepoCommand {
    /// List the loaded repositories (pretty-printed JSON)
    List,
    /// Initialise a new repository and print its UUID
    Init {
        root: PathBuf,
        /// External database location (instead of <root>/.metafolder)
        #[arg(long)]
        metafolder: Option<PathBuf>,
    },
    /// Load an existing repository and print its UUID
    Load {
        root: Option<PathBuf>,
        /// Load from an external .metafolder directory
        #[arg(long)]
        metafolder: Option<PathBuf>,
    },
    /// Unload the selected repository (stops its watcher, releases its DB lock)
    Unload,
}

#[derive(Subcommand)]
enum TaskCommand {
    /// List background tasks
    List {
        /// List tasks across all loaded repositories (no repo selector needed)
        #[arg(long)]
        all: bool,
        /// Print the raw JSON array
        #[arg(long)]
        json: bool,
    },
    /// Show a single background task by id (or stop it with --stop)
    Show {
        id: String,
        /// Request cancellation of the task instead of showing it (spec-tasks).
        #[arg(long, alias = "cancel")]
        stop: bool,
        /// Print the raw JSON object
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum LogCommand {
    /// Display the revision/operation history
    List {
        /// Show all branches as a flat list, not just the active line
        #[arg(long)]
        tree: bool,
        /// Draw every branch as an ASCII graph
        #[arg(long)]
        graph: bool,
        /// Expand each revision to show its individual operations
        #[arg(long)]
        ops: bool,
        /// Only revisions/ops that affected this metarecord
        #[arg(long)]
        metarecord: Option<String>,
        /// Show at most N revisions (or operations with --ops); default 20
        #[arg(long = "limit")]
        limit: Option<usize>,
        /// Only revisions with timestamp ≥ T (ISO-8601, or @<unix-ms>)
        #[arg(long)]
        since: Option<String>,
        /// Only revisions with timestamp ≤ T (ISO-8601, or @<unix-ms>)
        #[arg(long)]
        until: Option<String>,
        /// Remove the default limit of 20
        #[arg(long)]
        all: bool,
    },
    /// Show full details of one revision (a revision id, or HEAD)
    Show {
        target: String,
        /// Print the raw JSON response body
        #[arg(long)]
        raw: bool,
    },
    /// Navigate the history with coordinated filesystem moves
    Rollback {
        /// "plan" to preview, optionally a target label; or a target label.
        #[arg(num_args = 0..=2)]
        args: Vec<String>,
        /// Target operation by id
        #[arg(long)]
        id: Option<i64>,
        /// Target by revision timestamp (ISO-8601, or @<unix-ms>)
        #[arg(long)]
        timestamp: Option<String>,
        /// Policy when the file is present: apply|skip|abort|ask (default apply)
        #[arg(long = "on-move-available")]
        on_move_available: Option<String>,
        /// Policy when the file is missing: apply|skip|abort|ask (default ask)
        #[arg(long = "on-move-unavailable")]
        on_move_unavailable: Option<String>,
        /// Suppress informational output (still prompts for ask)
        #[arg(long)]
        silent: bool,
    },
    /// Permanently remove operations from the history (irreversible)
    Prune {
        #[command(subcommand)]
        command: PruneCommand,
    },
}

#[derive(Subcommand)]
enum MetarecordCommand {
    /// Read metarecords: UUID selector → full JSON; predicate/none → UUIDs
    Get {
        /// MetaRecord UUID or query predicate (omitted: all metarecords)
        selector: Option<String>,
        /// Print full metadata restricted to these fields, or '*' for all
        #[arg(long)]
        select: Option<String>,
        /// Sort key field[:asc|desc]; repeatable (predicate selectors only)
        #[arg(long = "sort")]
        sort: Vec<String>,
        /// Stop after N metarecords
        #[arg(long)]
        limit: Option<usize>,
        /// Print the selected field's raw values, one per line
        #[arg(long, requires = "select")]
        values: bool,
        /// Treat the selector as simplified-language text and expand it first
        #[arg(short = 's', long)]
        simplified: bool,
    },
    /// Create a metarecord with the given fields and print its UUID
    Add {
        /// Field spec name:type[=value]; repeatable
        #[arg(required = true)]
        specs: Vec<String>,
        /// Required to write mfr_* fields
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Replace the ENTIRE field set of a metarecord (force required)
    Set {
        /// MetaRecord UUID
        uuid: String,
        /// Field spec name:type[=value]; repeatable
        #[arg(required = true)]
        specs: Vec<String>,
        /// Mandatory: confirms the full-record overwrite
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Delete the matching metarecords (metadata and all fields)
    Delete {
        /// MetaRecord UUID or query predicate
        selector: String,
        /// Skip the confirmation prompt for predicate selectors
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Field operations scoped to the selected metarecord(s)
    Field {
        /// MetaRecord UUID or query predicate
        selector: String,
        #[command(subcommand)]
        verb: FieldVerb,
    },
}

#[derive(Subcommand)]
enum FieldVerb {
    /// Print the field's value(s)
    Get { name: String },
    /// Replace all rows of the field with the given value(s)
    Set {
        /// Field spec name:type[=value]; repeatable (multi-map set)
        #[arg(required = true)]
        specs: Vec<String>,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Append one row (inverse of delete)
    Add {
        /// Field spec name:type[=value]
        spec: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Remove the row(s) equal to the spec (inverse of add)
    Delete {
        /// Field spec name:type[=value]
        spec: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Remove the field entirely (every row → unknown)
    Unset {
        name: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Subcommand)]
enum FieldCommand {
    /// Print a field row by its id
    Get { id: i64 },
    /// Change a row's name and/or value in place, keeping its id
    Set {
        id: i64,
        /// Field spec name:type[=value] — the new name and value
        spec: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Delete a field row by its id
    Delete {
        id: i64,
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Subcommand)]
enum GuiCommand {
    /// Print the GUI state (pretty-printed JSON)
    Status,
    /// Print the active repository of the focused workspace
    Repo,
    /// Workspace (tab) management
    Workspace {
        #[command(subcommand)]
        command: GuiWorkspaceCommand,
    },
    /// Print or assign the slot layout ('-' = hidden slot)
    Layout {
        /// Slot name (left or right)
        slot: Option<String>,
        /// Workspace id to assign, or '-' to hide the slot
        value: Option<String>,
    },
    /// Print or set the panel type shown in a slot
    View {
        /// Slot name (left or right)
        slot: String,
        /// Panel type to set (omit to print the current one)
        panel_type: Option<String>,
        /// File path (file panel type)
        #[arg(long)]
        path: Option<String>,
        /// Initial panel state as a JSON object
        #[arg(long)]
        state: Option<String>,
    },
    /// Post a message to a workspace's status bar
    Message {
        text: String,
        /// Target workspace id (default: the focused workspace)
        #[arg(long)]
        workspace: Option<String>,
        /// Auto-clear delay; persistent when omitted
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Print the recorded bench measures (JSON), or clear the buffer
    Bench {
        /// Empty the bench buffer instead of printing it
        #[arg(long)]
        clear: bool,
    },
    /// Run a command invocation through the GUI (same as the command input)
    Command {
        /// The command invocation, e.g. `panel:set-type file`
        #[arg(required = true, trailing_var_arg = true)]
        invocation: Vec<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Wait for one of the given keys and print it
    Input {
        /// Keys to bind for the duration of the wait (e.g. y n escape)
        #[arg(required = true)]
        keys: Vec<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Prompt the user in the command input and print the answer
    Prompt {
        text: String,
        /// Autocomplete value offered during the prompt; repeatable
        #[arg(long = "completion")]
        completions: Vec<String>,
        /// Read more completions from stdin (one per line, empty line ends)
        #[arg(long)]
        completions_stdin: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
}

#[derive(Subcommand)]
enum GuiWorkspaceCommand {
    /// Create a workspace and print its id
    New {
        /// Active repository UUID (default: the daemon's first repo)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Close a workspace
    Rm { id: String },
}

/// A rollback/prune target: a revision label, --id, or --timestamp.
#[derive(clap::Args)]
struct TargetOpts {
    /// Revision label (most recent on the HEAD ancestry path)
    target: Option<String>,
    /// Target operation by id
    #[arg(long)]
    id: Option<i64>,
    /// Most recent operation whose revision timestamp ≤ T (ISO-8601, or @<unix-ms>)
    #[arg(long)]
    timestamp: Option<String>,
}

impl TargetOpts {
    fn into_args(self) -> metafolder_cli::log::TargetArgs {
        metafolder_cli::log::TargetArgs { label: self.target, id: self.id, timestamp: self.timestamp }
    }
    fn is_empty(&self) -> bool {
        self.target.is_none() && self.id.is_none() && self.timestamp.is_none()
    }
}

#[derive(Subcommand)]
enum PruneCommand {
    /// Make <target> the new root, deleting all older operations
    Before {
        #[command(flatten)]
        target: TargetOpts,
        /// Skip the confirmation prompt
        #[arg(long)]
        force: bool,
        /// Suppress informational output
        #[arg(long)]
        silent: bool,
    },
    /// Delete branch operations diverging from the HEAD path up to <target>
    Linearize {
        #[command(flatten)]
        target: TargetOpts,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        silent: bool,
    },
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Check metarecords against the schema and list the violations
    Check {
        /// Restrict the check to metarecords matching this predicate
        predicate: Option<String>,
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
    /// Re-read the schema file
    Reload,
    /// Print the loaded schema (pretty-printed JSON)
    Show,
}

fn main() {
    let cli = Cli::parse();
    let ctx = Ctx::new(cli.port, cli.repo_name, cli.repo_uuid);
    let result = dispatch(&ctx, cli.command);
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {}", error.message());
            std::process::exit(error.exit_code());
        }
    }
}

type CmdResult = Result<i32, metafolder_cli::client::CliError>;

fn dispatch(ctx: &Ctx, command: Command) -> CmdResult {
    match command {
        Command::Repo { command } => match command.unwrap_or(RepoCommand::List) {
            RepoCommand::List => commands::repos(ctx),
            RepoCommand::Init { root, metafolder } => {
                commands::init(ctx, &root, metafolder.as_deref())
            }
            RepoCommand::Load { root, metafolder } => {
                commands::load(ctx, root.as_deref(), metafolder.as_deref())
            }
            RepoCommand::Unload => commands::unload(ctx),
        },
        Command::Task { command } => {
            match command.unwrap_or(TaskCommand::List { all: false, json: false }) {
                TaskCommand::List { all, json } => commands::tasks(ctx, all, json),
                TaskCommand::Show { id, stop, json } => commands::task(ctx, &id, stop, json),
            }
        }
        Command::Log { command } => dispatch_log(ctx, command),
        Command::Metarecord { command } => dispatch_metarecord(ctx, command),
        Command::Field { command } => dispatch_field(ctx, command),
        Command::Retype { name, to } => commands::retype(ctx, &name, &to),
        Command::Reconcile {
            metarecord,
            threshold,
            no_mime,
            no_refresh,
            json,
            no_wait,
            poll_interval,
        } => commands::reconcile(
            ctx,
            metarecord.as_deref(),
            threshold,
            !no_mime,
            !no_refresh,
            json,
            no_wait,
            poll_interval,
        ),
        Command::Track { path } => commands::track(ctx, &path),
        Command::Path { uuid, relative } => commands::path(ctx, &uuid, relative),
        Command::Schema { command } => match command {
            SchemaCommand::Check { predicate, json } => {
                commands::schema_check(ctx, predicate.as_deref(), json)
            }
            SchemaCommand::Reload => commands::schema_reload(ctx),
            SchemaCommand::Show => commands::schema_show(ctx),
        },
        Command::Gui { gui_url, command } => dispatch_gui(gui_url, command),
    }
}

fn dispatch_metarecord(ctx: &Ctx, command: Option<MetarecordCommand>) -> CmdResult {
    match command {
        None => commands::metarecord_get(ctx, None, None, &[], None, false, false),
        Some(MetarecordCommand::Get { selector, select, sort, limit, values, simplified }) => {
            commands::metarecord_get(
                ctx,
                selector.as_deref(),
                select.as_deref(),
                &sort,
                limit,
                values,
                simplified,
            )
        }
        Some(MetarecordCommand::Add { specs, force }) => commands::create(ctx, &specs, force),
        Some(MetarecordCommand::Set { uuid, specs, force }) => {
            commands::metarecord_set(ctx, &uuid, &specs, force)
        }
        Some(MetarecordCommand::Delete { selector, force }) => {
            commands::delete(ctx, &selector, force)
        }
        Some(MetarecordCommand::Field { selector, verb }) => match verb {
            FieldVerb::Get { name } => commands::field_get(ctx, &selector, &name),
            FieldVerb::Set { specs, force } => commands::field_set(ctx, &selector, &specs, force),
            FieldVerb::Add { spec, force } => commands::add(ctx, &selector, &spec, force),
            FieldVerb::Delete { spec, force } => commands::remove(ctx, &selector, &spec, force),
            FieldVerb::Unset { name, force } => commands::field_unset(ctx, &selector, &name, force),
        },
    }
}

fn dispatch_field(ctx: &Ctx, command: FieldCommand) -> CmdResult {
    match command {
        FieldCommand::Get { id } => commands::field_by_id_get(ctx, id),
        FieldCommand::Set { id, spec, force } => commands::field_by_id_set(ctx, id, &spec, force),
        FieldCommand::Delete { id, force } => commands::field_by_id_delete(ctx, id, force),
    }
}

fn dispatch_log(ctx: &Ctx, command: Option<LogCommand>) -> CmdResult {
    match command {
        None => log::log(ctx, &log::LogArgs::default()),
        Some(LogCommand::List { tree, graph, ops, metarecord, limit, since, until, all }) => {
            log::log(ctx, &log::LogArgs { tree, graph, ops, metarecord, limit, since, until, all })
        }
        Some(LogCommand::Show { target, raw }) => log::log_show(ctx, &target, raw),
        Some(LogCommand::Rollback {
            args,
            id,
            timestamp,
            on_move_available,
            on_move_unavailable,
            silent,
        }) => {
            let (is_plan, label) = match args.split_first() {
                Some((first, rest)) if first == "plan" => (true, rest.first().cloned()),
                Some((first, _)) => (false, Some(first.clone())),
                None => (false, None),
            };
            let target = log::TargetArgs { label, id, timestamp };
            if is_plan {
                log::rollback_plan(ctx, target)
            } else {
                let policies = (|| {
                    Ok::<_, metafolder_cli::client::CliError>(log::RollbackPolicies {
                        on_available: on_move_available
                            .as_deref()
                            .map(log::Policy::parse)
                            .transpose()?
                            .unwrap_or(log::Policy::Apply),
                        on_unavailable: on_move_unavailable
                            .as_deref()
                            .map(log::Policy::parse)
                            .transpose()?
                            .unwrap_or(log::Policy::Ask),
                    })
                })();
                match policies {
                    Ok(policies) => log::rollback_run(ctx, target, policies, silent),
                    Err(e) => Err(e),
                }
            }
        }
        Some(LogCommand::Prune { command }) => match command {
            PruneCommand::Before { target, force, silent } => {
                if target.is_empty() {
                    Err(metafolder_cli::client::CliError::Usage(
                        "mf log prune before requires a target (<label>, --id, or --timestamp)".into(),
                    ))
                } else {
                    log::prune(ctx, "before", target.into_args(), force, silent)
                }
            }
            PruneCommand::Linearize { target, force, silent } => {
                if target.is_empty() {
                    Err(metafolder_cli::client::CliError::Usage(
                        "mf log prune linearize requires a target (<label>, --id, or --timestamp)"
                            .into(),
                    ))
                } else {
                    log::prune(ctx, "linearize", target.into_args(), force, silent)
                }
            }
        },
    }
}

fn dispatch_gui(gui_url: Option<String>, command: GuiCommand) -> CmdResult {
    let url = gui::base_url(gui_url, &gui::config_path_candidates());
    let gui_ctx = GuiCtx::new(&url);
    match command {
        GuiCommand::Status => gui::status(&gui_ctx),
        GuiCommand::Repo => gui::repo(&gui_ctx),
        GuiCommand::Workspace { command } => match command {
            GuiWorkspaceCommand::New { repo } => gui::workspace_new(&gui_ctx, repo.as_deref()),
            GuiWorkspaceCommand::Rm { id } => gui::workspace_rm(&gui_ctx, &id),
        },
        GuiCommand::Layout { slot, value } => {
            gui::layout(&gui_ctx, slot.as_deref(), value.as_deref())
        }
        GuiCommand::View { slot, panel_type, path, state } => {
            gui::view(&gui_ctx, &slot, panel_type.as_deref(), path.as_deref(), state.as_deref())
        }
        GuiCommand::Message { text, workspace, timeout_ms } => {
            gui::message(&gui_ctx, &text, workspace.as_deref(), timeout_ms)
        }
        GuiCommand::Bench { clear } => gui::bench(&gui_ctx, clear),
        GuiCommand::Command { invocation, timeout_ms } => {
            gui::command(&gui_ctx, &invocation.join(" "), timeout_ms)
        }
        GuiCommand::Input { keys, timeout_ms } => gui::input(&gui_ctx, &keys, timeout_ms),
        GuiCommand::Prompt { text, completions, completions_stdin, timeout_ms } => {
            gui::prompt(&gui_ctx, &text, &completions, completions_stdin, timeout_ms)
        }
    }
}
