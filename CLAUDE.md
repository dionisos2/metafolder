# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build all crates
cargo build

# Run all tests
cargo test

# Run tests for a single crate
cargo test -p metafolder-core
cargo test -p metafolder-daemon
cargo test -p metafolder-cli

# Run a single test by name (substring match)
cargo test -p metafolder-daemon test_reconcile_creates_for_new_files

# Run the daemon (default port 7523)
cargo run -p metafolder-daemon
cargo run -p metafolder-daemon -- --port 8080

# Run the CLI (requires daemon running)
cargo run -p metafolder-cli -- repo init /path/to/folder
cargo run -p metafolder-cli -- --repo <UUID> entries list
```

## Architecture

Cargo workspace with four crates: `core`, `daemon`, `cli`, `gui`.

### `crates/core`

Defines the shared data model used by all other crates:

- `entry.rs`: `Metadata` (uuid + db_id + Vec<Field>), `Field` (name + value), `Value` enum. Fields form a **multi-map**: multiple fields with the same name are allowed. `Value::Nothing` is an explicit absence ("this field does not apply"), distinct from the field simply not existing ("unknown"). All nine `Value` variants (Nothing, String, Int, Float, Bool, Date, DateTime, Duration, Ref) serialize to `{"type": "...", "value": ...}` JSON.
- `query.rs`: `Query` enum — the intermediate representation for queries. Includes boolean combinators (And/Or/Not), three-valued logic predicates (IsPresent/IsAbsent/IsUnknown), comparisons (Eq/Neq/Lt/Lte/Gt/Gte), and graph traversal (Follows / FollowsTransitive for following `Ref` fields).

### `crates/daemon`

Background HTTP server (Axum + Tokio) managing one or more repositories.

- `main.rs`: CLI args (clap), starts Axum on `127.0.0.1:<port>`.
- `config.rs`: `RepoConfig` — persisted as `.metafolder/config.json`. Contains `repo_uuid`, `version`, `root` path, `created_at`.
- `state.rs`: `AppState` holds a `Mutex<HashMap<Uuid, RepoState>>`. Each `RepoState` owns an `Arc<Mutex<rusqlite::Connection>>` and a `WatcherHandle`. `create_repo` / `load_repo` initialize or reopen a repository.
- `db.rs`: All SQLite operations. Schema uses an EAV (entity-attribute-value) pattern: `metadata` table (uuid, db_id) and `field` table (id, metadata_uuid, field_name, value_type, value_str, value_int, value_real, value_ref). UUIDs are stored as 16-byte BLOBs.
- `query_exec.rs`: Compiles a `Query` into a CTE-based SQL string using a `Compiler` that emits one CTE per query node. Recursive CTEs handle `FollowsTransitive`. Results are always filtered by `db_id` to isolate repositories sharing a database connection.
- `watcher.rs`: Uses the `notify` crate (inotify on Linux) to watch a repository root. Creates entries on `Create`, sets `path` to `Nothing` on `Remove`, updates `path` on `Rename(Both)`. Ignores events under `.metafolder/`.
- `routes.rs`: Axum route handlers. Blocking SQLite calls are dispatched via `tokio::task::spawn_blocking`. Key routes:
  - `GET /health`, `GET /repos`, `POST /repos/init`, `POST /repos/load`
  - `GET|POST /repos/:repo_uuid/entries`
  - `GET|DELETE|PATCH /repos/:repo_uuid/entries/:entry_uuid`
  - `POST /repos/:repo_uuid/query` — accepts a `Query` JSON body
  - `POST /repos/:repo_uuid/reconcile` — walks filesystem, creates/clears entries

### `crates/cli`

Clap-based subcommand tool that communicates with the daemon over HTTP.

- `main.rs`: Subcommands: `repo {init,load,list}`, `entries {create,get,list,set,delete}`, `query <predicate>`, `reconcile`. Repo UUID supplied via `--repo <UUID>` or `METAFOLDER_REPO` env var. Daemon URL via `--daemon-url` or `METAFOLDER_DAEMON_URL`.
- `dsl.rs`: Hand-written lexer + recursive-descent parser for the query DSL. Grammar: `field IS PRESENT|ABSENT|UNKNOWN`, comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`), `AND`/`OR`/`NOT`, parentheses, and graph traversal (`field -> atom`, `field ->* atom`).
- `http.rs`: `reqwest`-based async HTTP helpers for each daemon endpoint.
- `output.rs`: Pretty-printing for entries, repo lists, UUIDs, reconcile results.

### `crates/gui`

Stub only — not yet implemented.

## Repository structure on disk

Each watched directory gets a `.metafolder/` subdirectory containing:
- `config.json` — `RepoConfig` (uuid, version, root, created_at)
- `db.sqlite` — SQLite database with WAL mode and foreign keys enabled

## Key invariants

- A file entry has a `path` field (`Value::String`). When the file is deleted, `path` becomes `Value::Nothing` (the entry is preserved for its metadata).
- `db_id` on a `Metadata` entry equals the `repo_uuid` of the repository that owns it — used to isolate queries when multiple repos share a connection.
- `set_field` replaces **all** rows for `(uuid, field_name)` — it collapses multi-map fields to a single value.
