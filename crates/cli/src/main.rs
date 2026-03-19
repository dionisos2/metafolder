mod dsl;
mod http;
mod output;

use anyhow::bail;
use clap::{Parser, Subcommand};
use metafolder_core::entry::Value;
use uuid::Uuid;

// ── CLI structure ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "metafolder", about = "Metafolder CLI")]
struct Cli {
    #[arg(long, env = "METAFOLDER_DAEMON_URL", default_value = "http://localhost:7523")]
    daemon_url: String,

    #[arg(long, env = "METAFOLDER_REPO")]
    repo: Option<Uuid>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Repository management
    #[command(subcommand)]
    Repo(RepoCmd),
    /// Entry management
    #[command(subcommand)]
    Entries(EntriesCmd),
    /// Run a query
    Query {
        /// DSL predicate (e.g. "rating > 3 AND path IS PRESENT")
        predicate: String,
    },
    /// Sync the database with the filesystem
    Reconcile,
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Create and initialise a new repository
    Init {
        root: String,
    },
    /// Load an existing repository
    Load {
        root: String,
    },
    /// List loaded repositories
    List,
}

#[derive(Subcommand)]
enum EntriesCmd {
    /// Create a new entry
    Create {
        /// Field specification: name:type:value  (repeatable)
        #[arg(long = "field")]
        fields: Vec<String>,
    },
    /// Get an entry
    Get {
        uuid: Uuid,
    },
    /// List all entry UUIDs
    List,
    /// Set a field value on one entry (uuid) or all matching entries (--query)
    Set {
        /// UUID of the entry to update (mutually exclusive with --query)
        #[arg(long, value_name = "UUID", conflicts_with = "query_pred")]
        uuid: Option<Uuid>,
        /// DSL predicate: update all matching entries (mutually exclusive with UUID)
        #[arg(long = "query", value_name = "PREDICATE", conflicts_with = "uuid")]
        query_pred: Option<String>,
        field: String,
        #[arg(value_name = "TYPE")]
        type_str: String,
        value: String,
    },
    /// Delete an entry
    Delete {
        uuid: Uuid,
    },
}

// ── Value parsing helpers ─────────────────────────────────────────────────────

fn parse_value(type_str: &str, value_str: &str) -> anyhow::Result<Value> {
    match type_str {
        "string" => Ok(Value::String(value_str.to_string())),
        "int" => Ok(Value::Int(value_str.parse()?)),
        "float" => Ok(Value::Float(value_str.parse()?)),
        "bool" => Ok(Value::Bool(value_str.parse()?)),
        "nothing" => Ok(Value::Nothing),
        "ref" => Ok(Value::Ref(value_str.parse()?)),
        other => bail!("Unknown type: '{other}'"),
    }
}

/// Parse "name:type:value" — split on the first two ':' only.
fn parse_field_spec(spec: &str) -> anyhow::Result<(String, Value)> {
    let mut parts = spec.splitn(3, ':');
    let name = parts.next().ok_or_else(|| anyhow::anyhow!("Empty field spec"))?.to_string();
    let type_str = parts.next().ok_or_else(|| anyhow::anyhow!("Missing type in {spec}"))?;
    let value_str = parts.next().ok_or_else(|| anyhow::anyhow!("Missing value in {spec}"))?;
    Ok((name, parse_value(type_str, value_str)?))
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let base = &cli.daemon_url;

    match cli.command {
        Commands::Repo(cmd) => match cmd {
            RepoCmd::Init { root } => {
                let uuid = http::init_repo(base, &root).await?;
                println!("{uuid}");
            }
            RepoCmd::Load { root } => {
                let uuid = http::load_repo(base, &root).await?;
                println!("{uuid}");
            }
            RepoCmd::List => {
                let repos = http::list_repos(base).await?;
                output::print_repos(&repos);
            }
        },

        Commands::Entries(cmd) => {
            let repo = require_repo(cli.repo)?;
            match cmd {
                EntriesCmd::Create { fields } => {
                    let parsed: Vec<(String, Value)> = fields
                        .iter()
                        .map(|s| parse_field_spec(s))
                        .collect::<anyhow::Result<_>>()?;
                    let entry = http::create_entry(base, repo, parsed).await?;
                    output::print_metadata(&entry);
                }
                EntriesCmd::Get { uuid } => {
                    let entry = http::get_entry(base, repo, uuid).await?;
                    output::print_metadata(&entry);
                }
                EntriesCmd::List => {
                    let uuids = http::list_entries(base, repo).await?;
                    output::print_uuids(&uuids);
                }
                EntriesCmd::Set { uuid, query_pred, field, type_str, value } => {
                    let val = parse_value(&type_str, &value)?;
                    let uuids: Vec<Uuid> = match (uuid, query_pred) {
                        (Some(u), _) => vec![u],
                        (_, Some(pred)) => {
                            let q = dsl::parse(&pred)?;
                            http::query(base, repo, &q).await?
                        }
                        (None, None) => bail!(
                            "entries set requires either a UUID or --query <predicate>"
                        ),
                    };
                    for u in uuids {
                        let entry = http::set_field(base, repo, u, &field, val.clone()).await?;
                        output::print_metadata(&entry);
                    }
                }
                EntriesCmd::Delete { uuid } => {
                    http::delete_entry(base, repo, uuid).await?;
                    println!("deleted {uuid}");
                }
            }
        }

        Commands::Query { predicate } => {
            let repo = require_repo(cli.repo)?;
            let q = dsl::parse(&predicate)?;
            let uuids = http::query(base, repo, &q).await?;
            output::print_uuids(&uuids);
        }

        Commands::Reconcile => {
            let repo = require_repo(cli.repo)?;
            let r = http::reconcile(base, repo).await?;
            output::print_reconcile(r.created, r.cleared);
        }
    }

    Ok(())
}

fn require_repo(repo: Option<Uuid>) -> anyhow::Result<Uuid> {
    repo.ok_or_else(|| {
        anyhow::anyhow!("--repo <UUID> (or METAFOLDER_REPO env var) is required for this command")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entries_set_uuid_flag() {
        // After fix: `entries set --uuid <uuid> <field> <type> <value>` should parse correctly.
        // Before fix: clap panics because optional positional precedes required positionals.
        let result = Cli::try_parse_from([
            "metafolder",
            "--repo", "47247c18-e16b-4935-8582-3bd13c9ecb9f",
            "entries", "set",
            "--uuid", "f319bd20-60f2-425f-8270-c269f7d77210",
            "rating", "int", "9",
        ]);
        assert!(result.is_ok(), "entries set --uuid <uuid> should parse successfully");
    }
}
