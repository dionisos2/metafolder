//! `mf` binary: argument parsing (clap) and dispatch to
//! [`metafolder_cli::commands`]. Exit codes (spec-main): 0 success,
//! 1 operation failed, 2 usage error (clap also exits 2 on bad arguments).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use metafolder_cli::commands::{self, Ctx, QueryArgs};

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
    /// User schema commands
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
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
        Command::Query { predicate, select, sort, limit } => {
            commands::query(&ctx, &QueryArgs { predicate, select, sort, limit })
        }
        Command::Reconcile { entry, json } => commands::reconcile(&ctx, entry.as_deref(), json),
        Command::Track { path } => commands::track(&ctx, &path),
        Command::Schema { command } => match command {
            SchemaCommand::Check { predicate, json } => {
                commands::schema_check(&ctx, predicate.as_deref(), json)
            }
            SchemaCommand::Reload => commands::schema_reload(&ctx),
            SchemaCommand::Show => commands::schema_show(&ctx),
        },
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {}", error.message());
            std::process::exit(error.exit_code());
        }
    }
}
