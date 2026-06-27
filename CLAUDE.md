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

# Run the CLI (binary name: mf). Repo/daemon are chosen by global flags before
# the command: -n <name> / -u <uuid> (repo), -p <port> (daemon, default 7523).
cargo run -p metafolder-cli -- --help
cargo run -p metafolder-cli -- -u <UUID> metarecord get

# GUI: frontend tests + build (run from crates/gui/frontend; npm install once)
npm --prefix crates/gui/frontend test
npm --prefix crates/gui/frontend run build

# Run the GUI (binary name: metafolder-gui; build the frontend first)
cargo run -p metafolder-gui
cargo run -p metafolder-gui -- --gui-port 7524 --daemon-port 7523

# Install/update the user configuration repo at ~/.config/metafolder/
# (feature-gated: NOT built by a plain `cargo build`). Run from the checkout
# root; gathers crates/*/default-config/ and applies them via git.
cargo run -p metafolder-core --features sync-config --bin metafolder-sync-config
```

The daemon and GUI read their configuration from `~/.config/metafolder/<crate>/`
and **do not** install or fall back to embedded defaults: a missing or invalid
config file is a hard startup error. `metafolder-sync-config` must have run
first (see `docs/spec-config.org`).

Building the gui crate needs the Tauri system libraries (Arch:
`webkit2gtk-4.1 gtk3 librsvg`) and `npm`. Plain `cargo build` works without
a built frontend (`build.rs` writes a placeholder `frontend/dist`), but the
window will be empty until `npm run build` has produced the real bundle.

### Runtime dependencies (GUI)

External tools/assets the GUI shells out to or relies on at runtime. None are
needed to build or test; each degrades gracefully when absent (a missing one
disables its feature, it never crashes), but the feature silently looks broken
without it. Arch package names in parentheses.

- **An emoji-color font** (`noto-fonts-emoji`) — the `file-manager` row icons
  and the `thumbnail()` type glyphs (📁 🎬 🎵 📕 🖼️ 🗜️…) are emoji. Without an
  emoji font WebKit renders them as blank "tofu" boxes, so the icons appear
  missing even though the code is correct.
- **`ffmpeg`** — video poster thumbnails (`GET /thumbnail`). Absent ⇒ video
  tiles fall back to the 🎬 glyph.
- **GStreamer playback plugins** (`gst-plugins-good` for the
  `autoaudiosink`/`autovideosink` WebKit needs; `gst-libav` /
  `gst-plugins-bad` / `gst-plugins-ugly` for the actual decoders, e.g. H.264).
  The `file` panel probes `GET /__media-support` (sinks, to avoid a WebKit
  crash) and `gst-discoverer-1.0` (`gst-plugins-base`) per file to report
  missing decoders. Absent ⇒ the inline media preview is disabled with a
  message.

## Specs and roadmap

The implementation follows the specs under `docs/` (`spec-main.org`,
`spec-data-model.org`, `spec-query.org`, `spec-file-tracking.org`,
`spec-event-log.org`, `spec-schema.org`, `spec-platform.org`, `spec-config.org`;
`spec-sync.org` and parts tagged `:v2:` are deferred). `spec-indexing.org` is a
forward-looking *design* doc (indexing & query-performance architecture for
large repos: bitmap/BSI indexes, adaptive planner) — only its baseline indexes
are implemented. When changing daemon behaviour, check the relevant spec section
first; when deviating, update the spec.

## Architecture

Cargo workspace: `core`, `daemon`, `cli`, `gui` (Tauri v2 + Svelte 5), `bench`
(benchmark harness). The bench runs against two **persistent data folders**
(default `benchmarks/bench_data` and `benchmarks/bench_data_big`) so file-count
/ DB-size effects are comparable. The folders are consume-only: each must exist,
hold files, and not already contain a `.metafolder` (the run aborts otherwise);
the bench inits a repo in place, reconciles it to populate the DB from the real
files, benchmarks, and removes the `.metafolder` on teardown. Modes:
`data` (default — daemon-side CLI/query + watcher, the watcher renames files in
place and undoes it), `gui` (also launches a GUI window and runs the
`scripts/bench-gui.sh` scenarios on both repos, with a raw-HTTP baseline),
`attach` (drive an already-running GUI). `--small DIR`/`--big DIR` override the
folders. Select with `cargo run -p metafolder-bench -- data|gui|attach`.

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
- `config.rs`: std-only resolution of the user config paths
  (`~/.config/metafolder/<crate>/` from `$XDG_CONFIG_HOME`/`$HOME`, no `dirs`
  crate) + `read_required` (a missing config file is an error, never a fall
  back to a shipped default — spec-config "No runtime fallback").
- `config_sync.rs` + the `metafolder-sync-config` binary (behind the
  `sync-config` feature; `git2`/libgit2): the **only** git actor. Gathers
  `crates/*/default-config/` and applies them to the git-backed config repo —
  `default` branch = shipped defaults (merge base), `main` = user's working
  config; updates auto-commit dirty `main`, then merge `default`→`main`,
  restoring `main` untouched on conflict. See `docs/spec-config.org`.
- `simplified/`: the simplified-query language (engine/grammar/template + a
  `load` module reading `~/.config/metafolder/core/query-grammar`). Expansion
  (simplified → normal DSL) is a **pure, client-side** transformation: the GUI
  and CLI call it directly; the daemon never does (no `/query/expand`).
- `dsl.rs`: the query DSL parser (DSL text → `Query` IR), shared by the daemon,
  CLI and GUI. `date.rs`: the single dependency-free ISO-8601 ↔ Unix-ms helper.

### `crates/daemon`

Background HTTP server (Axum + Tokio) managing one or more repositories.
The crate is a library (`lib.rs`) plus a thin binary (`main.rs`); integration
tests live in `crates/daemon/tests/` and drive the Axum router directly with
`tower::ServiceExt::oneshot`.

- `config.rs`: `RepoConfig` persisted as `.metafolder/config.json`
  (repo_uuid, name, version, root, optional schema path, created_at).
- `daemon_config.rs`: optional daemon config
  `~/.config/metafolder/daemon/config.toml` (`--config` overrides; path from
  `core::config`), read at startup: `load` list of repos to auto-load
  (`POST /repos/load` shape); malformed file aborts startup, a repo that fails
  to load is a warning. The daemon does **not** handle the simplified-query
  grammar — expansion is client-side (see `crates/core` → `simplified/`).
- `repo.rs`: repository init/load (`OpenedRepo`), external `.metafolder/`
  location, the filesystem root metarecord with its defaults (`mf_watch = false`,
  default `mf_ignore` patterns), case-sensitivity probe.
- `db.rs`: SQLite layer. EAV schema: `metarecord` (uuid, version),
  `metarecord_db` (one row per owning repo), `field` (value columns incl.
  `value_uuid`/`value_ref_repo`/`value_name`; UNIQUE tree index on
  `(field_name, value_uuid, value_name)` for `tree_ref` rows) and the event
  log tables (`revision`, `operation`, `op_snapshot`, `log_head`,
  `pending_operation`) plus `field_text`, an FTS5 trigram virtual table
  (contentless, `rowid = field.id`) that pre-filters `Matches` (spec-query). It
  is maintained in the write transaction — upsert at the `insert_field_row`
  chokepoint (correct because `field.id` is AUTOINCREMENT, so a superset is
  always re-checked by REGEXP), best-effort deletes, `ensure_field_text`
  back-fill/rebuild on open. UUIDs are 16-byte BLOBs; the zero UUID is the
  TreeRef root sentinel. Connections: WAL (DELETE fallback for network FS),
  `locking_mode = EXCLUSIVE` (one daemon per repo), pattern-caching `REGEXP` UDF.
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
  field names. **Eagerly populated at repo load** (`populate`, one bulk scan of
  the forest, run by the background warmup — see `RepoState::warmup`) so
  read-side navigation — `resolve_path`, `descendants`,
  `path_of`/`paths_of` — is served entirely from memory (`is_complete()`); the
  DB walks remain only as a fallback when the forest exceeds the node budget or
  a drop-and-reload shortcut leaves the cache incomplete. O(1) rename/move, LRU
  eviction via a lazy min-heap of leaves. Manual API writes that change a
  TreeRef (`Writer::touched_tree`) rebuild the cache; the watcher/reconcile keep
  it in sync incrementally via `apply_*`. `path_of`/`paths_of` are exposed as
  `POST /repos/:repo/query/fields/resolve-tree` (set form, `{query, field}`) and
  `GET …/metarecords/:uuid/fields/:name/resolve-tree` (direct) so the CLI/GUI never re-walk the chain
  client-side.
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
- `fts.rs`: `required_fts_literal` — the sound, conservative regex→literal
  extraction (longest mandatory ≥3-char run from the parsed HIR) backing the
  `Matches` FTS5 trigram pre-filter; soundness cross-checked by
  `tests/fts_oracle.rs`.
- `fingerprint.rs`: xxHash3 partial (first+last 4 KiB) and full hashes.
- `fs_meta.rs`: stat-derived `mfr_*` fields (ISO-8601 via `core::date`).
- `reconcile.rs`: full reconcile (fs walk with eligibility pruning, the
  fingerprint phase with definitive moves and strong/weak candidates, metarecord
  creation; never writes `mfr_path = Nothing`) and single-metarecord reconcile.
- `query_exec.rs`: the SQL fallback engine — compiles a `Query` into a CTE
  chain (one CTE per node, `_repo` CTE = universe/isolation). `FollowsTransitive`
  is hybrid: descendants come from the tree cache and are inlined as literals.
  Sorting computes each metarecord's representative by joining the *filtered*
  universe `_res` to `field` (so the window is over the filtered rows only) and
  drives straight from `_s0`; spec semantics (unknown/Nothing last, multi-map
  min/max, fixed type-group precedence) via window functions; keyset pagination
  uses opaque cursors bound to a hash of (query, sort).
- `index/`: the in-memory bitmap/BSI accelerator (spec-indexing). **Built at
  repo load** by the background warmup (`RepoState::warmup`, a `load` task —
  see below) and refreshed to HEAD per query; `run_query_filter` (routes) tries it first and
  falls back to `query_exec` only on `Unsupported`. Serves predicates as
  RoaringBitmaps, `FollowsTransitive` by iterative bitmap expansion, sort from
  BSI representatives, `count` in O(1). Also backs `GET /repos/:repo/fields`
  (`field_catalog`: distinct field names + value types from the `present`/`types`
  maps, no DB scan). `Path`-target follows are resolved to a
  root metarecord through the (eager) tree cache by the caller and passed in as
  `PathRoots` (`evaluate_page_with_roots`/`count_with_roots`), so
  `mfr_path ->* "/dir"` + sort is fully in-memory; validated against the SQL
  engine by `tests/index_oracle.rs`.
- `schema.rs`: user schema parsing/validation (errors identify the offending
  constraint), per-field index, delta validation of user writes (violations
  roll the transaction back and return 400 with a `violations` array).
- `pagination.rs`, `error.rs`, `reserved.rs`: cursor encoding, the
  `{"error": ...}` JSON error type (status classification), reserved field
  rules (`mfr_*` need `force`; unknown `mf_*` rejected).
- `state.rs` + `routes.rs`: `AppState` (loaded repos), `RepoState` (conn,
  tree cache, schema, watcher/executor handles + a mutable `name` for rename),
  Axum handlers (blocking SQLite work via `spawn_blocking`). The API has two
  layers (spec-data-model/spec-query): *resources* for a single directly-
  addressed thing — `…/metarecords/:uuid`, per-record fields by name
  `…/metarecords/:uuid/fields/:name` (+ `/resolve-tree`), rows by id
  `…/fields/:id`, `…/retype`, `GET …/fields` (distinct field names + types,
  optional `?type=`), `GET …/tree/roots` (TreeRef forest roots, optional
  `?field=`), `GET`/`PATCH /repos/:repo` (info/rename) — and the
  *set layer* `POST …/query/*` (query, query/delete,
  query/fields/{set,append,remove,unset,resolve-tree}) where every body carries a
  `query`; a `uuid_in` predicate (core) targets an explicit set, so the two
  layers overlap only on n=1. `POST /repos/load` returns the uuid
  immediately and warms the repo (tree cache + index) in the background as an
  observable `load` task (`RepoState::warmup`), so the GUI shows a load
  progress bar; the repo answers via the DB fallback meanwhile. `init` and
  startup auto-load warm synchronously (no client is watching).

### `crates/cli`

The `mf` binary (package `metafolder-cli`): a thin client over the daemon's
HTTP API, specified in the `* CLI` sections of the spec files (the log,
rollback and prune commands are implemented in `log.rs`; only the sync
commands remain v2 and unimplemented). Library + thin `main.rs`.

The command tree (spec-data-model "* CLI") is a noun/verb hierarchy whose verbs
follow one pattern at every level — `get`⁻¹`set`, `add`⁻¹`delete`:

- `mf repo {list,init,load,unload}`, `mf task {list,show}`,
  `mf log {list,rollback,prune}` — `list` is each group's default.
- `mf metarecord [selector] <verb>` where the **selector is an option that
  precedes the verb** (so it can serve every verb, `field` included, without
  clap's "positional before subcommand" limit): `-q "<DSL>"` (query; add `-s`
  for simplified text), `-i <uuid>` (one metarecord), or none (all). Verbs:
  `get` (-i → full JSON; -q/none → uuids, `--select/--sort/--limit/--values`),
  `add <specs>` (create, no selector), `set <specs>` (whole-record overwrite —
  needs `-i` and mandatory `-f`), `delete` (needs a selector), and
  `field <verb> <name|spec>` (`get/set/add/delete/unset`, scoped by the
  selector). Example: `mf -n music metarecord -q 'rating>3' field set tag:string=x`.
- `mf field {list,get,set,delete}` — `list [--type <value_type>]` enumerates the
  repo's distinct field names + types (`GET …/fields`; the group default, so
  `mf field` ≡ `mf field list`); `get/set/delete <id>` are direct field-row
  access by DB id.
- `mf retype <name> <type>` (top-level; name-scoped, repo-wide).

main.rs holds only clap structs + `dispatch_*`; the work is in:

- `fieldspec.rs`: parses `name:type[=value]` field specs into `(String, Value)`.
- The query DSL parser is shared from core (`lib.rs` re-exports
  `metafolder_core::dsl`); the simplified→DSL expansion is also done locally
  via `metafolder_core::simplified` (no daemon round-trip).
- `client.rs`: `ureq`-based HTTP client; `CliError::Usage` (exit 2) vs
  `CliError::Op` (exit 1), daemon/GUI `{"error": ...}` bodies become
  `error: <message>` on stderr.
- `commands.rs`: one function per command; `Ctx` resolves the repo (`-n` name →
  uuid via `GET /repos`, cached) and builds the base URL from `-p`;
  `resolve_selector` turns `-q/-i/-s` into a target string (+ simplified
  expansion). Internal pagination (`PAGE_SIZE` 500, follows `next_cursor`),
  reconcile/violation formatting, confirmation prompt for query `delete`,
  `mf path` (one `POST /query/fields/resolve-tree` call, uuid_in query), `field get`/`metarecord get
  --values` (raw values, one per line).
- `gui.rs`: `mf gui …` — client for the GUI scripting API (spec-gui "CLI:
  mf gui"): status/repo/workspace/layout/view/message/input/prompt, GUI
  port discovery via the GUI `config.toml` (`gui-port`), `--gui-url`/`METAFOLDER_GUI_URL`.
- `log.rs`: `mf log {list,show,rollback,prune}` (+ `rollback plan`, with the
  `--on-move-available/-unavailable` skip policies driving the coordinated
  navigation). ISO-8601 ↔ Unix-ms conversion is shared from
  `metafolder_core::date` (the single dependency-free date helper, also used by
  the daemon's `fs_meta`).

Repo selection (`-n <name>` / `-u <uuid>`, mutually exclusive) and the daemon
port (`-p`, default 7523) are **global flags that precede the command** — not
clap-`global`, so they appear only in `mf --help`, not in every subcommand's
help. Env fallbacks: `METAFOLDER_REPO_NAME`, `METAFOLDER_REPO`,
`METAFOLDER_DAEMON_PORT`. A repo-scoped command with no selector fails with a
usage error (exit 2) before any HTTP round-trip.

### `crates/gui`

The `metafolder-gui` binary (package `metafolder-gui`): a Tauri v2 desktop app over
the daemon HTTP API, specified in `docs/spec-gui.org`. **Rust owns all
canonical state**; the Svelte 5 shell (`frontend/`) mirrors it via Tauri
events; panel types are plain HTML/JS directories that run **in the shell's JS
realm**, each mounted in a Shadow DOM root via `export function mount(root,
metafolder)` (no iframes — panels are trusted code; this enables a future
shared data cache). `index.html` is markup only; `main.js` is the entry.

- `state/`: `GuiState` — workspaces (tabs), the two panel slots, focus,
  per-workspace variables and message logs. Every mutation emits an event
  through the `FrontendNotifier` trait (`notifier.rs`; tests use
  `RecordingNotifier`).
- `keybindings.rs`: combo/sequence parsing, TOML model, single-file +
  panel-suggestion merge, compiled table consumed by the single shell-side JS
  matcher (`panel-shim/keymatch.js`; panel key events bubble through the Shadow
  DOM to the shell, so text-input detection uses `composedPath()`).
- `command_registry.rs`: builtin + panel-registered commands, autocomplete
  listing (global + focused panel's local commands).
- `config.rs`: `~/.config/metafolder/gui/` — reads `config.toml` (`GuiConfig`:
  `daemon-port`, default the daemon's default port 7523 — the GUI connects on
  127.0.0.1 — + `gui-port`, default 7524; CLI flags `--daemon-port`/`--gui-port`
  override), the keybindings, stylesheet and panel types that
  `metafolder-sync-config` installed (no install, mirror or embedded fallback
  here; missing files error). `keybindings.toml` is the **complete** set
  (single-file model: `set` upserts, `remove` unbinds, and reverting to a
  default is a git op on the config repo). The GUI binds the configured
  `gui-port` (the CLI reads the same file — no `gui.port` discovery file).
  Shipped defaults live in `crates/gui/default-config/`.
- `server/`: Axum router on 127.0.0.1:7524 (permissive CORS so the shell can
  fetch/`import()` panel files cross-origin) — panel assets served verbatim
  (`panel_assets.rs`), raw files with Range support (`fsraw.rs`),
  and the `/gui/*` scripting API (`gui_api.rs`, `input_wait.rs`: workspaces,
  layout, panel views, message, input/prompt waits with a single lock, 409
  on concurrency, status snapshot).
- `daemon_proxy.rs`: reqwest client to the daemon (the WebView cannot call
  it directly — CORS); health polling emits `daemon-health-changed`.
- `commands.rs`: thin `#[tauri::command]` wrappers; `shell_exec.rs` (`!`
  commands), `style_watcher.rs` (style.css auto-reload), `fs_commands.rs`
  (`metafolder.fs`), `reconcile.rs` (`reconcile:run` flow).
- `frontend/src/lib/panels/api.ts`: `createPanelApi(deps, ctx)` builds the
  `metafolder` object passed to each panel's `mount` (daemon/workspace/commands/
  fs/statusBar/messages + `addKeybinding` + `cache`), calling Tauri commands
  directly; per-instance var/message/visibility push registries.
- `frontend/src/lib/panels/cache.ts`: the single in-realm daemon-data cache
  (`createCache`, a shared singleton) — entity / TreeRef-path / query /
  field-catalog stores (the last: distinct field names + types from
  `GET /repos/:repo/fields`, re-warmed on sync, `fieldType` lets a form lock the
  type picker to an existing field's only valid type),
  `metafolder.cache.*` fetch+read API (sync `readMetarecord`/`readTreeRef`/
  `readFields`/`fieldType` return `REFRESH` when absent), transparent interception under `daemon.call`,
  invalidation from the `GET /log/since` change feed (polled in `initStore` +
  at query/refresh/display), write-invalidation, and LRU pruning. Validated by
  oracle/equivalence tests (`tests/cache-oracle.test.ts`).
  `panel-shim/` holds the framework-free helpers it reuses: `resolve.js`
  (memoized `query/fields/resolve-tree` paths), `visibility.js`, `menu.js`, `keymatch.js`;
  `ui.js` (served as `/__ui.js`, imported by panels): `el()` DOM builder,
  `formatValue()`, `byName()`/`field()`/`fields()` (memoized field index).
- `default-config/panel-types/`: the built-in panels (repos, metarecord-list,
  metarecord-detail, file, file-manager, message, log, workspace-info, hello
  example) as plain HTML/JS, shipped into the user config repo by
  `metafolder-sync-config`.
- `frontend/`: Svelte shell — TabBar, two Slots + divider, CommandInput
  (per-workspace drafts, autocomplete, script prompts), StatusBar(s),
  ConfigOverlay (paths + keybinding editor) and PanelHost (`lib/panels/
  PanelHost.svelte`: one Shadow-DOM host per workspace×panel-type, never
  reparented — fetches `index.html` into the shadow, `import()`s `main.js` and
  calls `mount(root, api)`; a shell-side registry routes panel commands).

GUI tests: `cargo test -p metafolder-gui` (state/keybindings/registry unit
tests + `tests/*.rs` driving the Axum router with oneshot and a stub daemon
on an ephemeral port); `npm --prefix crates/gui/frontend test` (vitest:
keymatch, commands parsing, panel `api`, resolve).

GUI dev gotchas (learned the hard way):
- Panels are served from `~/.config/metafolder/gui/panel-types/` (the config
  repo's `main` branch). To pick up new built-in panel code after editing the
  source, re-run `metafolder-sync-config`: it merges the shipped `default`
  branch into `main` (a real git 3-way merge, conflicts restore `main`
  untouched). The GUI itself never installs or upgrades anything at startup.
- Panels run in the shell's realm: a panel exports `mount(root, metafolder)`
  where `root` is its Shadow DOM root — use `root.getElementById(...)`, not
  `document` (which is the shell). `body`-level CSS must target
  `.mf-panel-body`. Return a cleanup fn to remove any `document`-level
  listeners. There is no iframe/process isolation: a throw in `mount` is
  caught by the host's error boundary, but a panel can still corrupt shared
  state — panels are trusted code.
- Dynamic `import('/panel/X/main.js')` is cached per URL; PanelHost cache-busts
  with `?v=<session>` so an edited panel reloads on GUI restart, but a sub-module
  edit (`./columns.js`) is not hot-reloaded. Re-run `metafolder-sync-config`
  after editing built-in panel sources (they are served from the config repo).

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

The **user configuration** (distinct from per-repo data) is a single git
repository at `~/.config/metafolder/`, one subdirectory per crate
(`core/`, `daemon/`, `gui/`, `cli/`), with a `default` branch (shipped
defaults / merge base) and a `main` branch (the user's edits, the only branch
read at runtime). Managed exclusively by `metafolder-sync-config`; the shipped
sources are `crates/*/default-config/`. See `docs/spec-config.org`.

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
