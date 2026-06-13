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
cargo test -p metafolder-daemon test_reconcile_creates_records_for_new_files

# Run the daemon (default port 7523)
cargo run -p metafolder-daemon
cargo run -p metafolder-daemon -- --port 8080

# Run the CLI (binary name: mf)
cargo run -p metafolder-cli -- --help
cargo run -p metafolder-cli -- --repo <UUID> list

# GUI: frontend tests + build (run from crates/gui/frontend; npm install once)
npm --prefix crates/gui/frontend test
npm --prefix crates/gui/frontend run build

# Run the GUI (binary name: mf-gui; build the frontend first)
cargo run -p metafolder-gui
cargo run -p metafolder-gui -- --gui-port 7524 --daemon-url http://127.0.0.1:7523
```

Building the gui crate needs the Tauri system libraries (Arch:
`webkit2gtk-4.1 gtk3 librsvg`) and `npm`. Plain `cargo build` works without
a built frontend (`build.rs` writes a placeholder `frontend/dist`), but the
window will be empty until `npm run build` has produced the real bundle.

## Specs and roadmap

The implementation follows the specs under `docs/` (`spec-main.org`,
`spec-data-model.org`, `spec-query.org`, `spec-file-tracking.org`,
`spec-event-log.org`, `spec-schema.org`, `spec-platform.org`; `spec-sync.org`
and parts tagged `:v2:` are deferred). When changing daemon behaviour, check
the relevant spec section first; when deviating, update the spec.

## Architecture

Cargo workspace: `core`, `daemon`, `cli`, `gui` (Tauri v2 + Svelte 5), `bench`
(old POC harness, to be revived with the CLI).

### `crates/core`

Shared data model used by all other crates:

- `metarecord.rs`: `MetaRecord` (uuid + `db_ids: Vec<Uuid>` + `version: u64` + fields),
  `Field` (`id: Option<i64>` — the DB row id, present in API responses — +
  name + value), `Value` enum. Fields form a **multi-map**: several fields
  with the same name are allowed. `Value::Nothing` is an explicit absence
  ("this field does not apply"), distinct from the field not existing
  ("unknown"). Ten `Value` variants: `Nothing`, `String`, `Int`, `Float`,
  `Bool`, `DateTime`, `Ref(Uuid)`, `TreeRef { parent: Option<Uuid>, name }`,
  `RefBase(Uuid)`, `ExternalRef { repo, metarecord }`. JSON form is
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
- `daemon_config.rs`: optional daemon config
  `~/.config/metafolder/config.json` (`--config` overrides), read at
  startup: `load` list of repos to auto-load (`POST /repos/load` shape);
  malformed file aborts startup, a repo that fails to load is a warning.
- `repo.rs`: repository init/load (`OpenedRepo`), external `.metafolder/`
  location, the filesystem root metarecord with its defaults (`mf_watch = false`,
  default `mf_ignore` patterns), case-sensitivity probe.
- `db.rs`: SQLite layer. EAV schema: `metarecord` (uuid, version),
  `metarecord_db` (one row per owning repo), `field` (value columns incl.
  `value_uuid`/`value_ref_repo`/`value_name`; UNIQUE tree index on
  `(field_name, value_uuid, value_name)` for `tree_ref` rows) and the event
  log tables (`revision`, `operation`, `op_snapshot`, `log_head`,
  `pending_operation`). UUIDs are 16-byte BLOBs; the zero UUID is the
  TreeRef root sentinel. Connections: WAL (DELETE fallback for network FS),
  `locking_mode = EXCLUSIVE` (one daemon per repo), `REGEXP` UDF.
- `log.rs`: **all writes go through `Writer`** — one revision per Writer, one
  `operation` row + before/after `op_snapshot` rows per change, running HEAD
  chain, `metarecord.version` bump per field write. Also: history reading,
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
  fingerprint phase with definitive moves and strong/weak candidates, metarecord
  creation; never writes `mfr_path = Nothing`) and single-metarecord reconcile.
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
HTTP API, specified in the `* CLI` sections of the spec files (the log,
rollback and prune commands are implemented in `log.rs`; only the sync
commands remain v2 and unimplemented). Library + thin `main.rs`:

- `fieldspec.rs`: parses `name:type[=value]` field specs into
  `(String, Value)`.
- `dsl.rs`: hand-written lexer + recursive-descent parser compiling the
  query DSL (`rating > 3 AND genre = "jazz"`, `->`, `->*`, `IS PRESENT`...)
  to the `Query` JSON IR.
- `client.rs`: `ureq`-based HTTP client; `CliError::Usage` (exit 2) vs
  `CliError::Op` (exit 1), daemon/GUI `{"error": ...}` bodies become
  `error: <message>` on stderr.
- `commands.rs`: one function per command; `<query|uuid>` targets, internal
  pagination (`PAGE_SIZE` 500, follows `next_cursor`), reconcile/violation
  formatting, confirmation prompt for predicate `mf delete`, `mf path`
  (mfr_path chain walk), `mf query --values` (raw values, one per line).
- `gui.rs`: `mf gui …` — client for the GUI scripting API (spec-gui "CLI:
  mf gui"): status/repo/workspace/layout/view/message/input/prompt, GUI
  port discovery via the `gui.port` file, `--gui-url`/`METAFOLDER_GUI_URL`.
- `log.rs`: `mf log` / `mf log show`, `mf rollback` (+ `rollback plan`, with
  the `--on-move-available/-unavailable` skip policies driving the coordinated
  navigation), `mf prune before|linearize`, and a dependency-free ISO-8601 ↔
  Unix-ms date helper for timestamp targets and revision display.

Repo-scoped commands require `--repo`/`METAFOLDER_REPO` (checked before any
HTTP round-trip); `--daemon-url`/`METAFOLDER_DAEMON_URL` defaults to
`http://127.0.0.1:7523`.

### `crates/gui`

The `mf-gui` binary (package `metafolder-gui`): a Tauri v2 desktop app over
the daemon HTTP API, specified in `docs/spec-gui.org`. **Rust owns all
canonical state**; the Svelte 5 shell (`frontend/`) mirrors it via Tauri
events; panel types are plain HTML/JS directories rendered in iframes.

- `state/`: `GuiState` — workspaces (tabs), the two panel slots, focus,
  per-workspace variables and message logs. Every mutation emits an event
  through the `FrontendNotifier` trait (`notifier.rs`; tests use
  `RecordingNotifier`).
- `keybindings.rs`: combo/sequence parsing, TOML model, defaults + user +
  panel-suggestion merge, compiled table consumed by the shared JS matcher
  (`panel-shim/keymatch.js`, used identically by the shell and inside each
  iframe — key events do not cross iframe boundaries).
- `command_registry.rs`: builtin + panel-registered commands, autocomplete
  listing (global + focused panel's local commands).
- `config.rs`: `~/.config/metafolder-gui/` — first-run install of editable
  defaults (`keybindings.toml`, `style.css`, `panel-types/*`; user edits are
  never overwritten), always-refreshed `panel-types-defaults/` diff mirror,
  user keybinding override persistence, `gui.port` discovery file.
- `server/`: Axum router on 127.0.0.1:7524 — panel assets with shim+style
  injection (`panel_assets.rs`), raw files with Range support (`fsraw.rs`),
  and the `/gui/*` scripting API (`gui_api.rs`, `input_wait.rs`: workspaces,
  layout, panel views, message, input/prompt waits with a single lock, 409
  on concurrency, status snapshot).
- `daemon_proxy.rs`: reqwest client to the daemon (the WebView cannot call
  it directly — CORS); health polling emits `daemon-health-changed`.
- `commands.rs`: thin `#[tauri::command]` wrappers; `shell_exec.rs` (`!`
  commands), `style_watcher.rs` (style.css auto-reload), `fs_commands.rs`
  (`metafolder.fs`), `reconcile.rs` (`reconcile:run` flow).
- `panel-shim/shim.js`: injected into every panel document; provides
  `window.metafolder` (daemon/workspace/commands/fs/statusBar/messages +
  `addKeybinding`) over a postMessage protocol; `resolve.js`: memoized lazy
  TreeRef path resolution; `ui.js` (served as `/__ui.js`, importable by
  panels): `el()` DOM builder, `formatValue()`, `field()`.
- `panel-types/`: the built-in panels (repos, metarecord-list, metarecord-detail,
  file, file-manager, message, log, workspace-info, hello example) as
  user-copyable plain HTML/JS.
- `frontend/`: Svelte shell — TabBar, two Slots + divider, CommandInput
  (per-workspace drafts, autocomplete, script prompts), StatusBar(s),
  ConfigOverlay (paths + keybinding editor), PanelHost (iframe pool, one
  live iframe per workspace×panel-type, never reparented) and the bridge
  (`lib/panels/bridge.ts`, unit-tested core of the postMessage protocol).

GUI tests: `cargo test -p metafolder-gui` (state/keybindings/registry unit
tests + `tests/*.rs` driving the Axum router with oneshot and a stub daemon
on an ephemeral port); `npm --prefix crates/gui/frontend test` (vitest:
keymatch, commands parsing, bridge, resolve).

GUI dev gotchas (learned the hard way):
- Panels are served from `~/.config/metafolder-gui/panel-types/` (the user
  copy). Never-edited copies (identical to the `panel-types-defaults/`
  mirror) are auto-upgraded at startup; a copy the user has edited is never
  overwritten and must be deleted by hand to pick up new built-in panel
  code. Launching a stale binary is harmless since the next fresh launch
  upgrades the copies it left behind.
- Never post `$state` proxies through `postMessage` (DataCloneError, and
  the rejection is silent in an async handler): `$state.snapshot()` first.
- WebKitGTK swaps the iframe WindowProxy on cross-origin navigation:
  never key anything on a captured `contentWindow`; match `event.source`
  against the live `iframe.contentWindow` at message time (and the shim
  re-sends `ready` until the shell answers).

## Repository structure on disk

Each repository's `.metafolder/` directory (inside the watched root, or at an
external location recorded in the config) contains:
- `config.json` — `RepoConfig`
- `schema.json` — optional user schema (spec-schema)
- `internal/db.sqlite` — SQLite database (WAL, exclusive lock while loaded)

`internal/` (database + sidecars, case probe) is the only part of
`.metafolder/` excluded from tracking — by absolute path, in both the watcher
and the reconcile walk; the rest of `.metafolder/` is ordinary trackable
content. A pre-`internal/` layout is migrated automatically at load
(`db.sqlite*` moved into `internal/`).

## Key invariants

- A file metarecord has an `mfr_path` field (`Value::TreeRef`). When the file is
  deleted, `mfr_path` becomes `Value::Nothing` (the metarecord is preserved).
  Only the watcher and manual `force` writes set `mfr_path` to `Nothing`;
  reconcile leaves stale TreeRefs in place.
- Repository ownership lives in `metarecord_db`; queries return only metarecords
  owned exclusively by the current repository.
- Tracking is opt-in: nothing is watched until `mf_watch = true` is set
  (inherited through the `mfr_path` tree, filtered by `mf_ignore` regexes).
- `set_field` replaces **all** rows for `(uuid, field_name)`.
- `metarecord.version` is managed exclusively by the daemon; incremented on
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
