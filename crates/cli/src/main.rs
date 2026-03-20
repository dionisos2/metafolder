mod dsl;
mod field_spec;
mod http;
mod output;

use clap::{Parser, Subcommand};
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
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a new repository at <root>
    Init {
        root: String,
    },
    /// Load an existing repository at <root>
    Load {
        root: String,
    },
    /// List loaded repositories (JSON)
    Repos,
    /// Sync the database with the filesystem
    Reconcile,
    /// List all entry UUIDs (one per line)
    List,
    /// Run a DSL query, print matching UUIDs (one per line)
    Query {
        /// DSL predicate (e.g. "rating > 3 AND path IS PRESENT")
        predicate: String,
    },
    /// Get full entry data for matching entries (JSON array)
    Get {
        /// DSL predicate or UUID
        query: String,
        /// Comma-separated field names to include (default: all)
        #[arg(long)]
        fields: Option<String>,
    },
    /// Create a new entry (prints its UUID)
    Create {
        /// Field specification: name:type=value  (repeatable)
        #[arg(long = "field")]
        fields: Vec<String>,
    },
    /// Set a field on all entries matching <query>
    Set {
        /// DSL predicate or UUID
        query: String,
        /// Field spec: name:type=value
        field_spec: String,
    },
    /// Delete entries matching <query>
    Delete {
        /// DSL predicate or UUID
        query: String,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let base = &cli.daemon_url;

    match cli.command {
        Cmd::Init { root } => {
            let uuid = http::init_repo(base, &root).await?;
            println!("{uuid}");
        }

        Cmd::Load { root } => {
            let uuid = http::load_repo(base, &root).await?;
            println!("{uuid}");
        }

        Cmd::Repos => {
            let repos = http::list_repos(base).await?;
            println!("{}", serde_json::to_string_pretty(&repos)?);
        }

        Cmd::Reconcile => {
            let repo = require_repo(cli.repo)?;
            let r = http::reconcile(base, repo).await?;
            println!("created: {}  cleared: {}", r.created, r.cleared);
        }

        Cmd::List => {
            let repo = require_repo(cli.repo)?;
            let uuids = http::list_entries(base, repo).await?;
            for u in uuids {
                println!("{u}");
            }
        }

        Cmd::Query { predicate } => {
            let repo = require_repo(cli.repo)?;
            let q = dsl::parse(&predicate)?;
            let uuids = http::query(base, repo, &q).await?;
            for u in uuids {
                println!("{u}");
            }
        }

        Cmd::Get { query, fields } => {
            let repo = require_repo(cli.repo)?;
            let field_filter: Option<Vec<String>> = fields
                .map(|s| s.split(',').map(|f| f.trim().to_string()).collect());
            let uuids = resolve_uuids(base, repo, &query).await?;
            let mut entries = Vec::with_capacity(uuids.len());
            for uuid in uuids {
                let entry = http::get_entry(base, repo, uuid).await?;
                entries.push(output::filter_entry(entry, field_filter.as_deref()));
            }
            println!("{}", serde_json::to_string_pretty(&entries)?);
        }

        Cmd::Create { fields } => {
            let repo = require_repo(cli.repo)?;
            let parsed: Vec<metafolder_core::entry::Field> = fields
                .iter()
                .map(|s| field_spec::parse(s))
                .collect::<anyhow::Result<_>>()?;
            let entry = http::create_entry(base, repo, parsed).await?;
            println!("{}", entry.uuid);
        }

        Cmd::Set { query, field_spec: spec } => {
            let repo = require_repo(cli.repo)?;
            let field = field_spec::parse(&spec)?;
            let uuids = resolve_uuids(base, repo, &query).await?;
            for uuid in uuids {
                http::set_field(base, repo, uuid, &field.name, field.value.clone()).await?;
            }
        }

        Cmd::Delete { query } => {
            let repo = require_repo(cli.repo)?;
            let uuids = resolve_uuids(base, repo, &query).await?;
            for uuid in uuids {
                http::delete_entry(base, repo, uuid).await?;
            }
        }
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn require_repo(repo: Option<Uuid>) -> anyhow::Result<Uuid> {
    repo.ok_or_else(|| {
        anyhow::anyhow!("--repo <UUID> (or METAFOLDER_REPO env var) is required for this command")
    })
}

/// Resolves a query string to a list of UUIDs.
/// If the string is a bare UUID, returns it directly.
/// Otherwise parses it as a DSL predicate and queries the daemon.
async fn resolve_uuids(base: &str, repo: Uuid, query: &str) -> anyhow::Result<Vec<Uuid>> {
    if let Ok(uuid) = query.parse::<Uuid>() {
        return Ok(vec![uuid]);
    }
    let q = dsl::parse(query).map_err(|e| {
        anyhow::anyhow!("Invalid query (not a UUID and failed to parse as DSL): {e}")
    })?;
    http::query(base, repo, &q).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmd_init_parses() {
        let result = Cli::try_parse_from(["metafolder", "init", "/tmp/myrepo"]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cmd_get_with_fields_parses() {
        let result = Cli::try_parse_from([
            "metafolder",
            "--repo",
            "47247c18-e16b-4935-8582-3bd13c9ecb9f",
            "get",
            "path IS PRESENT",
            "--fields",
            "path,rating",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cmd_set_parses() {
        let result = Cli::try_parse_from([
            "metafolder",
            "--repo",
            "47247c18-e16b-4935-8582-3bd13c9ecb9f",
            "set",
            "f319bd20-60f2-425f-8270-c269f7d77210",
            "rating:int=9",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cmd_create_parses() {
        let result = Cli::try_parse_from([
            "metafolder",
            "--repo",
            "47247c18-e16b-4935-8582-3bd13c9ecb9f",
            "create",
            "--field",
            "rating:int=5",
            "--field",
            "label:string=jazz",
        ]);
        assert!(result.is_ok());
    }
}
