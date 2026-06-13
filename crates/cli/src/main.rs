//! `mf` binary: argument parsing (clap) and dispatch to
//! [`metafolder_cli::commands`]. Exit codes (spec-main): 0 success,
//! 1 operation failed, 2 usage error (clap also exits 2 on bad arguments).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use metafolder_cli::commands::{self, Ctx, QueryArgs};
use metafolder_cli::gui::{self, GuiCtx};
use metafolder_cli::log;

#[derive(Parser)]
#[command(name = "mf", about = "metafolder CLI — thin client over the daemon HTTP API")]
struct Cli {
    /// Daemon base URL
    #[arg(
        long,
        global = true,
        env = "METAFOLDER_DAEMON_URL",
        default_value = "http://127.0.0.1:7523"
    )]
    daemon_url: String,

    /// Target repository UUID (required by repo-scoped commands)
    #[arg(long, global = true, env = "METAFOLDER_REPO")]
    repo: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    /// List the loaded repositories (pretty-printed JSON)
    Repos,
    /// Print all metarecord UUIDs of the repository, one per line
    List {
        /// Stop after N UUIDs
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print the full metadata of the matching metarecords (JSON array)
    Get {
        /// MetaRecord UUID or query predicate
        target: String,
        /// Only include the listed field names (comma-separated)
        #[arg(long, value_delimiter = ',')]
        fields: Option<Vec<String>>,
    },
    /// Create a metarecord with the given fields and print its UUID
    Create {
        /// Field spec name:type[=value]; repeatable
        #[arg(long = "field", required = true)]
        fields: Vec<String>,
        /// Required to write mfr_* fields
        #[arg(long)]
        force: bool,
    },
    /// Set a field (replaces all rows of that name)
    Set {
        /// MetaRecord UUID or query predicate
        target: String,
        /// Field spec name:type[=value]
        spec: String,
        /// Required to write mfr_* fields
        #[arg(long)]
        force: bool,
    },
    /// Append one field row without touching existing rows (multi-map)
    Add {
        uuid: String,
        /// Field spec name:type[=value]
        spec: String,
        /// Required to write mfr_* fields
        #[arg(long)]
        force: bool,
    },
    /// Delete a single field row by id (as shown by mf get)
    Unset {
        uuid: String,
        field_id: i64,
        /// Required to delete mfr_* fields
        #[arg(long)]
        force: bool,
    },
    /// Delete the matching metarecords (metadata and all fields)
    Delete {
        /// MetaRecord UUID or query predicate
        target: String,
        /// Skip the confirmation prompt for predicate targets
        #[arg(long)]
        force: bool,
    },
    /// Run a query predicate and print the matching metarecords
    Query {
        predicate: String,
        /// Print full metadata restricted to these fields, or '*' for all
        #[arg(long)]
        select: Option<String>,
        /// Sort key field[:asc|desc]; repeatable for secondary keys
        #[arg(long = "sort")]
        sort: Vec<String>,
        /// Stop after N results
        #[arg(long)]
        limit: Option<usize>,
        /// Print the selected field's raw values, one per line
        #[arg(long, requires = "select")]
        values: bool,
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
    /// Display the revision/operation history
    Log {
        /// `mf log show <N|HEAD>` — full details of one revision
        #[command(subcommand)]
        show: Option<LogShow>,
        /// Show all branches, not just the HEAD ancestry path
        #[arg(long)]
        tree: bool,
        /// Expand each revision to show its individual operations
        #[arg(long)]
        ops: bool,
        /// Only revisions/ops that affected this metarecord
        #[arg(long)]
        metarecord: Option<String>,
        /// Show at most N revisions (or operations with --ops); default 20
        #[arg(long = "limit", short = 'n')]
        limit: Option<usize>,
        /// Only revisions with timestamp ≥ T (ISO-8601 or Unix ms)
        #[arg(long)]
        since: Option<String>,
        /// Only revisions with timestamp ≤ T (ISO-8601 or Unix ms)
        #[arg(long)]
        until: Option<String>,
        /// Remove the default limit of 20
        #[arg(long)]
        all: bool,
    },
    /// Navigate the history with coordinated filesystem moves
    Rollback {
        /// "plan" to preview, optionally a target label; or a target label.
        /// Omitted: undo the last revision.
        #[arg(num_args = 0..=2)]
        args: Vec<String>,
        /// Target operation by id
        #[arg(long)]
        id: Option<i64>,
        /// Target by revision timestamp (ISO-8601 or Unix ms)
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
    /// User schema commands
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Drive the GUI through its scripting HTTP API
    Gui {
        /// GUI base URL (default: discovered via the gui.port file)
        #[arg(long, env = "METAFOLDER_GUI_URL")]
        gui_url: Option<String>,
        #[command(subcommand)]
        command: GuiCommand,
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

#[derive(Subcommand)]
enum LogShow {
    /// Show full details of one revision (a revision id, or HEAD)
    Show {
        target: String,
        /// Print the raw JSON response body
        #[arg(long)]
        raw: bool,
    },
}

/// A rollback/prune target: a revision label, --id, or --timestamp.
#[derive(clap::Args)]
struct TargetOpts {
    /// Revision label (most recent on the HEAD ancestry path)
    target: Option<String>,
    /// Target operation by id
    #[arg(long)]
    id: Option<i64>,
    /// Most recent operation whose revision timestamp ≤ T (ISO-8601 or Unix ms)
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
    let ctx = Ctx::new(&cli.daemon_url, cli.repo);
    let result = match cli.command {
        Command::Init { root, metafolder } => commands::init(&ctx, &root, metafolder.as_deref()),
        Command::Load { root, metafolder } => {
            commands::load(&ctx, root.as_deref(), metafolder.as_deref())
        }
        Command::Repos => commands::repos(&ctx),
        Command::List { limit } => commands::list(&ctx, limit),
        Command::Get { target, fields } => commands::get(&ctx, &target, fields.as_deref()),
        Command::Create { fields, force } => commands::create(&ctx, &fields, force),
        Command::Set { target, spec, force } => commands::set(&ctx, &target, &spec, force),
        Command::Add { uuid, spec, force } => commands::add(&ctx, &uuid, &spec, force),
        Command::Unset { uuid, field_id, force } => {
            commands::unset(&ctx, &uuid, field_id, force)
        }
        Command::Delete { target, force } => commands::delete(&ctx, &target, force),
        Command::Query { predicate, select, sort, limit, values } => {
            commands::query(&ctx, &QueryArgs { predicate, select, sort, limit, values })
        }
        Command::Reconcile { metarecord, threshold, no_mime, no_refresh, json } => {
            commands::reconcile(&ctx, metarecord.as_deref(), threshold, !no_mime, !no_refresh, json)
        }
        Command::Track { path } => commands::track(&ctx, &path),
        Command::Path { uuid, relative } => commands::path(&ctx, &uuid, relative),
        Command::Log { show, tree, ops, metarecord, limit, since, until, all } => match show {
            Some(LogShow::Show { target, raw }) => log::log_show(&ctx, &target, raw),
            None => log::log(
                &ctx,
                &log::LogArgs { tree, ops, metarecord, limit, since, until, all },
            ),
        },
        Command::Rollback {
            args,
            id,
            timestamp,
            on_move_available,
            on_move_unavailable,
            silent,
        } => {
            let (is_plan, label) = match args.split_first() {
                Some((first, rest)) if first == "plan" => (true, rest.first().cloned()),
                Some((first, _)) => (false, Some(first.clone())),
                None => (false, None),
            };
            let target = log::TargetArgs { label, id, timestamp };
            if is_plan {
                log::rollback_plan(&ctx, target)
            } else {
                let policies = || -> Result<log::RollbackPolicies, metafolder_cli::client::CliError> {
                    Ok(log::RollbackPolicies {
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
                }();
                match policies {
                    Ok(policies) => log::rollback_run(&ctx, target, policies, silent),
                    Err(e) => Err(e),
                }
            }
        }
        Command::Prune { command } => match command {
            PruneCommand::Before { target, force, silent } => {
                if target.is_empty() {
                    Err(metafolder_cli::client::CliError::Usage(
                        "mf prune before requires a target (<label>, --id, or --timestamp)".into(),
                    ))
                } else {
                    log::prune(&ctx, "before", target.into_args(), force, silent)
                }
            }
            PruneCommand::Linearize { target, force, silent } => {
                if target.is_empty() {
                    Err(metafolder_cli::client::CliError::Usage(
                        "mf prune linearize requires a target (<label>, --id, or --timestamp)".into(),
                    ))
                } else {
                    log::prune(&ctx, "linearize", target.into_args(), force, silent)
                }
            }
        },
        Command::Schema { command } => match command {
            SchemaCommand::Check { predicate, json } => {
                commands::schema_check(&ctx, predicate.as_deref(), json)
            }
            SchemaCommand::Reload => commands::schema_reload(&ctx),
            SchemaCommand::Show => commands::schema_show(&ctx),
        },
        Command::Gui { gui_url, command } => {
            let url = gui::base_url(gui_url, &gui::port_file_candidates());
            let gui_ctx = GuiCtx::new(&url);
            match command {
                GuiCommand::Status => gui::status(&gui_ctx),
                GuiCommand::Repo => gui::repo(&gui_ctx),
                GuiCommand::Workspace { command } => match command {
                    GuiWorkspaceCommand::New { repo } => {
                        gui::workspace_new(&gui_ctx, repo.as_deref())
                    }
                    GuiWorkspaceCommand::Rm { id } => gui::workspace_rm(&gui_ctx, &id),
                },
                GuiCommand::Layout { slot, value } => {
                    gui::layout(&gui_ctx, slot.as_deref(), value.as_deref())
                }
                GuiCommand::View { slot, panel_type, path, state } => gui::view(
                    &gui_ctx,
                    &slot,
                    panel_type.as_deref(),
                    path.as_deref(),
                    state.as_deref(),
                ),
                GuiCommand::Message { text, workspace, timeout_ms } => {
                    gui::message(&gui_ctx, &text, workspace.as_deref(), timeout_ms)
                }
                GuiCommand::Input { keys, timeout_ms } => {
                    gui::input(&gui_ctx, &keys, timeout_ms)
                }
                GuiCommand::Prompt { text, completions, completions_stdin, timeout_ms } => {
                    gui::prompt(&gui_ctx, &text, &completions, completions_stdin, timeout_ms)
                }
            }
        }
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {}", error.message());
            std::process::exit(error.exit_code());
        }
    }
}
