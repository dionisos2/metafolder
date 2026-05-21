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

- `entry.rs`: `Metadata` (uuid + db_ids: Vec<Uuid> + version + Vec<Field>), `Field` (name + value), `Value` enum. Fields form a **multi-map**: multiple fields with the same name are allowed. `Value::Nothing` is an explicit absence ("this field does not apply"), distinct from the field simply not existing ("unknown"). The ten `Value` variants (Nothing, String, Int, Float, Bool, DateTime, Ref(Uuid), TreeRef(Option<Uuid>, String), RefBase(Uuid), ExternalRef(Uuid, Uuid)) serialize to `{"type": "...", "value": ...}` JSON. `db_ids` normally contains one repo UUID; two UUIDs means a link entry shared between repos.
- `query.rs`: `Query` enum — the intermediate representation for queries. Includes boolean combinators (And/Or/Not), three-valued logic predicates (IsPresent/IsAbsent/IsUnknown), comparisons (Eq/Neq/Lt/Lte/Gt/Gte), `Matches` (regex on string/TreeRef), and graph traversal (`Follows` / `FollowsTransitive` for `Ref` and `TreeRef` fields).

### `crates/daemon`

Background HTTP server (Axum + Tokio) managing one or more repositories.

- `main.rs`: CLI args (clap), starts Axum on `127.0.0.1:<port>`.
- `config.rs`: `RepoConfig` — persisted as `.metafolder/config.json`. Contains `repo_uuid`, `version`, `root` path, `created_at`.
- `state.rs`: `AppState` holds a `Mutex<HashMap<Uuid, RepoState>>`. Each `RepoState` owns an `Arc<Mutex<rusqlite::Connection>>` and a `WatcherHandle`. `create_repo` / `load_repo` initialize or reopen a repository.
- `db.rs`: All SQLite operations. Schema uses an EAV (entity-attribute-value) pattern: `metadata` table (uuid, version), `metadata_db` table (metadata_uuid, db_id — one row per owning repo), and `field` table (id, metadata_uuid, field_name, value_type, value_text, value_int, value_real, value_uuid, value_ref_repo, value_name). UUIDs are stored as 16-byte BLOBs. Also contains the event log tables: `revision`, `operation`, `op_snapshot`, `log_head`, `pending_operation`.
- `query_exec.rs`: Compiles a `Query` into a CTE-based SQL string using a `Compiler` that emits one CTE per query node. `FollowsTransitive` on a `TreeRef` field is handled as a hybrid: descendant UUIDs are collected via the tree cache, then injected as an `IN` clause. Results are filtered by `metadata_db` to isolate the repository.
- `watcher.rs`: Uses the `notify` crate (inotify on Linux) to watch a repository root. Creates entries on `Create`, sets `mfr_path` to `Nothing` on `Remove`, updates `mfr_path` TreeRef on `Rename(Both)`. Writes events to the `pending_operation` table; the executor flushes them after a 500 ms quiet period. Ignores events under `.metafolder/`.
- `routes.rs`: Axum route handlers. Blocking SQLite calls are dispatched via `tokio::task::spawn_blocking`. Key routes:
  - `GET /health`, `GET /repos`, `POST /repos/init`, `POST /repos/load`
  - `GET|POST /repos/:repo_uuid/metadata`
  - `GET|DELETE|PATCH /repos/:repo_uuid/metadata/:metadata_uuid`
  - `POST /repos/:repo_uuid/metadata/:metadata_uuid/fields`
  - `PUT|DELETE /repos/:repo_uuid/metadata/:metadata_uuid/fields/:field_id`
  - `POST /repos/:repo_uuid/query`, `POST /repos/:repo_uuid/set`
  - `POST /repos/:repo_uuid/reconcile`
  - `GET /repos/:repo_uuid/log`, `GET|PATCH /repos/:repo_uuid/log/revisions/:rev_id`
  - `POST /repos/:repo_uuid/log/prune`, `POST /repos/:repo_uuid/rollback`

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

## Spec file organisation

All spec files under `docs/` follow the same top-level structure:

```
* Overview        — one-paragraph description of the topic
* Concepts        — shared data model / vocabulary, not component-specific
* Daemon          — daemon implementation: SQLite schema, internal logic, HTTP API
* CLI             — CLI-specific behaviour (stub if not yet specified)
* Open questions  — unresolved design questions
```

Within `* Daemon`, the typical sub-sections are:
- `** SQLite Schema` — tables and indexes; snapshot formats and invariants as `***` sub-sections
- `** <Feature>` — one sub-section per major behaviour (e.g. Navigation, Log pruning, Coordinated navigation)
- `** HTTP API` — all endpoints as `***` sub-sections, with request/response examples

Features deferred to v2 are tagged `:v2:` on their heading.

This structure allows reconstructing the full spec for a given component (e.g. daemon) by collecting all `* Daemon` sections across spec files.

## Key invariants

- A file entry has an `mfr_path` field (`Value::TreeRef`). When the file is deleted, `mfr_path` becomes `Value::Nothing` (the entry is preserved for its metadata). Only the watcher and manual `force` writes set `mfr_path` to `Nothing`; reconcile leaves stale TreeRefs in place.
- Repository ownership is recorded in the `metadata_db` table (`metadata_uuid`, `db_id`). Queries filter by `db_id` to isolate the current repository. Entries with two `db_id` rows are link entries shared between repos.
- `set_field` replaces **all** rows for `(uuid, field_name)` — it collapses multi-map fields to a single value.
- The `version` counter on a metadata entry is managed exclusively by the daemon; it is incremented on every write to `fields`.
- Every write to the data tables is recorded in the event log (`operation` + `op_snapshot`). HEAD (`log_head.op_id`) always reflects the current database state.
