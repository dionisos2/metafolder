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

# Run a single integration test binary / a single test by name
cargo test -p metafolder-daemon --test storage
cargo test -p metafolder-daemon test_reconcile_creates_entries_for_new_files

# Run the daemon (default port 7523)
cargo run -p metafolder-daemon
cargo run -p metafolder-daemon -- --port 8080

# Run the CLI (binary name: mf)
cargo run -p metafolder-cli -- --help
cargo run -p metafolder-cli -- --repo <UUID> list
```

## Specs and roadmap

The implementation follows the specs under `docs/` (`spec-main.org`,
`spec-data-model.org`, `spec-query.org`, `spec-file-tracking.org`,
`spec-event-log.org`, `spec-schema.org`, `spec-platform.org`; `spec-sync.org`
and parts tagged `:v2:` are deferred). When changing daemon behaviour, check
the relevant spec section first; when deviating, update the spec.

## Architecture

Cargo workspace: `core`, `daemon`, `cli`, `gui` (stub), `bench`
(old POC harness, to be revived with the CLI).

### `crates/core`

Shared data model used by all other crates:

- `entry.rs`: `Metadata` (uuid + `db_ids: Vec<Uuid>` + `version: u64` + fields),
  `Field` (`id: Option<i64>` — the DB row id, present in API responses — +
  name + value), `Value` enum. Fields form a **multi-map**: several fields
  with the same name are allowed. `Value::Nothing` is an explicit absence
  ("this field does not apply"), distinct from the field not existing
  ("unknown"). Ten `Value` variants: `Nothing`, `String`, `Int`, `Float`,
  `Bool`, `DateTime`, `Ref(Uuid)`, `TreeRef { parent: Option<Uuid>, name }`,
  `RefBase(Uuid)`, `ExternalRef { repo, entry }`. JSON form is
  `{"type": "...", "value": ...}` with UUIDs as 32-char lowercase hex (serde
  helpers `hex_uuid`, `hex_uuid_opt`, `hex_uuid_vec`).
- `query.rs`: `Query` enum — the JSON IR for queries (internally tagged with
  `"type"`, snake_case). Boolean combinators (And/Or/Not), three-valued
  predicates (IsPresent/IsAbsent/IsUnknown), comparisons (Eq/Neq/Lt/Lte/Gt/Gte),
  `Matches` (regex), and traversal: `Follows { field, target }` where target
  is a path string (TreeRef) or a sub-query (Ref), and
  `FollowsTransitive { field, path }` (TreeRef only).

### `crates/daemon`

Background HTTP server (Axum + Tokio) managing one or more repositories.
The crate is a library (`lib.rs`) plus a thin binary (`main.rs`); integration
tests live in `crates/daemon/tests/` and drive the Axum router directly with
`tower::ServiceExt::oneshot`.

- `config.rs`: `RepoConfig` persisted as `.metafolder/config.json`
  (repo_uuid, name, version, root, optional schema path, created_at).
- `repo.rs`: repository init/load (`OpenedRepo`), external `.metafolder/`
  location, the filesystem root entry with its defaults (`mf_watch = false`,
  default `mf_ignore` patterns), case-sensitivity probe.
- `db.rs`: SQLite layer. EAV schema: `metadata` (uuid, version),
  `metadata_db` (one row per owning repo), `field` (value columns incl.
  `value_uuid`/`value_ref_repo`/`value_name`; UNIQUE tree index on
  `(field_name, value_uuid, value_name)` for `tree_ref` rows) and the event
  log tables (`revision`, `operation`, `op_snapshot`, `log_head`,
  `pending_operation`). UUIDs are 16-byte BLOBs; the zero UUID is the
  TreeRef root sentinel. Connections: WAL (DELETE fallback for network FS),
  `locking_mode = EXCLUSIVE` (one daemon per repo), `REGEXP` UDF.
- `log.rs`: **all writes go through `Writer`** — one revision per Writer, one
  `operation` row + before/after `op_snapshot` rows per change, running HEAD
  chain, `metadata.version` bump per field write. Also: history reading,
  target resolution (id/timestamp/label/prev_revision), atomic navigation
  (LCA, inverse/forward application restoring original `field.id`s and
  versions), pruning (before/linearize). Watcher-originated writes use the
  file op types (`file_deleted`, `file_moved`, `file_modified`) but are
  set-field-shaped: one operation per field name (slight divergence from the
  spec's one-op-per-event snapshot table, chosen so every op has a uniform
  inverse).
- `tree_cache.rs`: per-repo in-memory path→UUID cache shared across TreeRef
  field names; lazy population from the DB, O(1) rename/move, LRU eviction
  via a lazy min-heap of leaves; descendant collection walks the DB.
- `eligibility.rs`: watch/ignore algorithm (`mf_watch` inherited, direct
  override, nearest-ancestor `mf_ignore` pattern set, no merging).
- `watcher.rs` + `executor.rs`: the watcher (notify/inotify) enqueues raw
  events into `pending_operation` and pings the executor, which flushes after
  a 500 ms quiet period: compaction (incl. absorbing notify's From/To/Both
  rename triplet), grouping by resulting op type (one revision per group),
  then application of the event semantics (cascading Nothing on removes,
  fingerprint search of orphans on arrivals). The buffer is replayed at repo
  load. Both hold `Weak<RepoState>` (an Arc would leak the repo and its
  exclusive lock).
- `fingerprint.rs`: xxHash3 partial (first+last 4 KiB) and full hashes.
- `fs_meta.rs`: stat-derived `mfr_*` fields, dependency-free ISO-8601.
- `reconcile.rs`: full reconcile (fs walk with eligibility pruning, the
  fingerprint phase with definitive moves and strong/weak candidates, entry
  creation; never writes `mfr_path = Nothing`) and single-entry reconcile.
- `query_exec.rs`: compiles a `Query` into a CTE chain (one CTE per node,
  `_repo` CTE = universe/isolation). `FollowsTransitive` is hybrid:
  descendants come from the tree cache and are inlined as literals. Sorting
  implements the spec semantics (unknown/Nothing last, multi-map min/max,
  fixed type-group precedence) via window functions; keyset pagination uses
  opaque cursors bound to a hash of (query, sort).
- `schema.rs`: user schema parsing/validation (errors identify the offending
  constraint), per-field index, delta validation of user writes (violations
  roll the transaction back and return 400 with a `violations` array).
- `pagination.rs`, `error.rs`, `reserved.rs`: cursor encoding, the
  `{"error": ...}` JSON error type (status classification), reserved field
  rules (`mfr_*` need `force`; unknown `mf_*` rejected).
- `state.rs` + `routes.rs`: `AppState` (loaded repos), `RepoState` (conn,
  tree cache, schema, watcher/executor handles), Axum handlers (blocking
  SQLite work via `spawn_blocking`).

### `crates/cli`

The `mf` binary (package `metafolder-cli`): a thin client over the daemon's
HTTP API, specified in the `* CLI` sections of the spec files (the
log/rollback/prune and sync commands are v2 and not implemented). Library +
thin `main.rs`:

- `fieldspec.rs`: parses `name:type[=value]` field specs into
  `(String, Value)`.
- `dsl.rs`: hand-written lexer + recursive-descent parser compiling the
  query DSL (`rating > 3 AND genre = "jazz"`, `->`, `->*`, `IS PRESENT`...)
  to the `Query` JSON IR.
- `client.rs`: `ureq`-based HTTP client; `CliError::Usage` (exit 2) vs
  `CliError::Op` (exit 1), daemon `{"error": ...}` bodies become
  `error: <message>` on stderr.
- `commands.rs`: one function per command; `<query|uuid>` targets, internal
  pagination (`PAGE_SIZE` 500, follows `next_cursor`), reconcile/violation
  formatting, confirmation prompt for predicate `mf delete`.

Repo-scoped commands require `--repo`/`METAFOLDER_REPO` (checked before any
HTTP round-trip); `--daemon-url`/`METAFOLDER_DAEMON_URL` defaults to
`http://127.0.0.1:7523`.

### `crates/gui`

Stub, not yet implemented.

## Repository structure on disk

Each repository's `.metafolder/` directory (inside the watched root, or at an
external location recorded in the config) contains:
- `config.json` — `RepoConfig`
- `db.sqlite` — SQLite database (WAL, exclusive lock while loaded)
- `schema.json` — optional user schema (spec-schema)

## Key invariants

- A file entry has an `mfr_path` field (`Value::TreeRef`). When the file is
  deleted, `mfr_path` becomes `Value::Nothing` (the entry is preserved).
  Only the watcher and manual `force` writes set `mfr_path` to `Nothing`;
  reconcile leaves stale TreeRefs in place.
- Repository ownership lives in `metadata_db`; queries return only entries
  owned exclusively by the current repository.
- Tracking is opt-in: nothing is watched until `mf_watch = true` is set
  (inherited through the `mfr_path` tree, filtered by `mf_ignore` regexes).
- `set_field` replaces **all** rows for `(uuid, field_name)`.
- `metadata.version` is managed exclusively by the daemon; incremented on
  every field write, restored exactly by rollback.
- Every write goes through `log::Writer`; HEAD (`log_head.op_id`) and the
  data tables are always mutually consistent (single transaction).
- TreeRef references form a forest per field name (no cycles, depth ≤ 1000);
  writes violating this are rejected with 400.
- The repository database is held under an exclusive SQLite lock for the
  whole lifetime of the connection: a second daemon cannot load the same
  repo, and `RepoState` must never be kept alive by its own background tasks
  (they hold `Weak` references).

## Testing conventions

TDD: write the failing test first. Unit tests live next to the code
(`#[cfg(test)]`); most coverage is in `crates/daemon/tests/*.rs` (one file
per feature area: `storage`, `repo`, `tree_cache`, `http_api`, `query`,
`query_http`, `eligibility`, `fs_meta`, `executor`, `watcher_e2e`,
`reconcile`, `track_http`, `schema`, `log_http`). HTTP tests build the router
with a fresh `AppState` and `oneshot` requests; filesystem tests use
disposable directories under `std::env::temp_dir()`.

CLI tests: parser and formatter unit tests live in the `cli` modules; e2e
tests (`crates/cli/tests/cli_e2e.rs`) run the real `mf` binary
(`env!("CARGO_BIN_EXE_mf")`) against one shared in-process daemon bound to an
ephemeral port.
