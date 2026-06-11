//! `mf` binary: argument parsing (clap) and dispatch to
//! [`metafolder_cli::commands`]. Exit codes (spec-main): 0 success,
//! 1 operation failed, 2 usage error (clap also exits 2 on bad arguments).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use metafolder_cli::commands::{self, Ctx, QueryArgs};
use metafolder_cli::gui::{self, GuiCtx};

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
    /// Print all entry UUIDs of the repository, one per line
    List {
        /// Stop after N UUIDs
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print the full metadata of the matching entries (JSON array)
    Get {
        /// Entry UUID or query predicate
        target: String,
        /// Only include the listed field names (comma-separated)
        #[arg(long, value_delimiter = ',')]
        fields: Option<Vec<String>>,
    },
    /// Create an entry with the given fields and print its UUID
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
        /// Entry UUID or query predicate
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
    /// Delete the matching entries (metadata and all fields)
    Delete {
        /// Entry UUID or query predicate
        target: String,
        /// Skip the confirmation prompt for predicate targets
        #[arg(long)]
        force: bool,
    },
    /// Run a query predicate and print the matching entries
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
        /// Single-entry reconcile, scoped to this entry's subtree
        #[arg(long)]
        entry: Option<String>,
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
    /// Create the entry for a single path and print its UUID
    Track { path: PathBuf },
    /// Print the filesystem path of an entry (walks the mfr_path chain)
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
enum SchemaCommand {
    /// Check entries against the schema and list the violations
    Check {
        /// Restrict the check to entries matching this predicate
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
        Command::Reconcile { entry, json } => commands::reconcile(&ctx, entry.as_deref(), json),
        Command::Track { path } => commands::track(&ctx, &path),
        Command::Path { uuid, relative } => commands::path(&ctx, &uuid, relative),
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
