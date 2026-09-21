# Changelog

All notable changes to metafolder are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
[Semantic Versioning](https://semver.org/) once the API stabilises.

**Stability:** the HTTP API, the CLI command tree and the configuration keys
still change between versions without deprecation. **On-disk formats are
preserved, though** — from v0.3 on, any change to a persisted format (the
SQLite schema, the `.metafolder/` layout, the config file format) ships with a
conversion path that migrates existing repositories and configuration rather
than breaking them.

## [Unreleased]

### Performance
- **Reading the log back is bounded again.** A listing of the most recent
  operations read every row of the `revision` table to build its timestamp map,
  so the cost of showing fifty operations grew with the whole log: 330 ms on a
  repository with 200 000 revisions, where the window itself is 12 KB. It now
  reads only the revisions of the operations it returns (15 ms on the same log).
- **`mf log list` asks for what it displays.** It fetched the *entire* log and
  trimmed it to twenty revisions locally — seven seconds of daemon time and two
  minutes of formatting on that same repository. `GET /log` gained a
  `revisions=N` parameter (whole revisions, most recent first), which is what
  the command now sends: 14 ms.
- A bounded log read that also filters (`metarecord_uuid`, `since`, `until`)
  now widens its walk until it has what was asked for: asking for the last ten
  operations of one metarecord answers with them instead of with "none of the
  repository's last ten operations are yours".

### Added
- **Performance-regression suite** (`docs/spec-perf.org`), in two layers:
  - *cost assertions* (`crates/daemon/tests/perf_cost.rs`), which count SQL
    statements and read query plans instead of a clock — they run in the
    ordinary `cargo test` pass and cannot flake under load;
  - *timed benchmarks* (`scripts/bench.sh`, `metafolder-bench regression`) over
    generated repositories, with a per-machine history kept in
    `benchmarks/history/<machine>.jsonl` and every run compared against the
    median of the last five.

## [0.3.0] — 2026-08-16

First tagged release. Summarises the capability set built since the initial
proof of concept.

### Data model & queries
- Universal **metarecord** model: a UUID plus a multi-map of `(name, value)`
  fields over ten value types (`nothing`, `string`, `int`, `float`, `bool`,
  `datetime`, `ref`, `tree_ref`, `refbase`, `external_ref`), with three-valued
  logic (present / explicitly absent / unknown).
- **Query DSL** and JSON IR: boolean combinators, three-valued predicates,
  comparisons, regex `matches`, ordered-substring matching, `uuid_in`, and
  reference traversal (`->`, `->*`) over the `tree_ref` forest.
- **Simplified query language**: a user-editable grammar that expands
  client-side into the normal DSL (shared by the CLI and GUI).
- Reserved fields: daemon-owned `mfr_*` (require `force` to override) and the
  `mf_*` controls (`mf_watch`, `mf_ignore`, `mf_schema`, `mf_sync`).
- Optional per-repository **user schema** with strict write validation and
  read-side violation reporting.

### Daemon
- Axum + Tokio HTTP server managing one or more repositories over a REST API,
  with a resource layer (single addressed thing) and a set layer
  (`POST …/query/*`).
- **SQLite** EAV storage (WAL, exclusive lock) with a full **event log**:
  every write goes through one `Writer`, one revision per write, with atomic
  metadata-only rollback, coordinated (watcher-suspended) navigation for file
  moves, history reading and pruning.
- **Filesystem watcher** (inotify) with a batched, compacted pending-event
  pipeline, and **reconcile** with fingerprint-based move detection.
- In-memory **tree cache** (path↔UUID, O(1) rename/move) and a bitmap/BSI
  **query accelerator**, both warmed in the background at repo load as an
  observable task.
- FTS5 trigram pre-filter for regex `matches`; embedded media metadata
  extraction into `mfr_meta_*`; MIME detection.
- **Session-token authentication** (spec-auth): every daemon and GUI request
  is gated by a per-service token in a user-only runtime file, keeping browser
  content out.

### CLI (`mf`)
- `repo`, `metarecord` (query/id/simplified selectors, `field` verbs),
  `field`, `retype`, `reconcile`/`track`/`path`, `log` (list/show/rollback/
  prune), `task`, `schema`, and `trash`.
- `tag` — hierarchical tags with subsumption and exclusivity; `order` — number
  a folder's children; `sync` — cross-repository synchronisation
  (plan/run/show/status/link/unlink).
- `gui` — drive a running GUI through its scripting API.

### GUI (`metafolder-gui`)
- Tauri v2 + Svelte 5 desktop app: workspaces (tabs), two panel slots, a
  keybinding system, a command input with autocomplete and interactive
  argument collection, input history, and a local `/gui/*` scripting API.
- Built-in panel types (plain HTML/JS in Shadow DOM roots): repos,
  metarecord-list, metarecord-detail, file (with sandboxed media preview),
  file-manager, treeref, ref-list, sync, trash, recent, log, message, help.
- A shared in-realm daemon-data cache with change-feed invalidation; shared
  file operations (cut/copy/paste/rename/duplicate/trash) in every panel's
  context menu; a recently-viewed picker.
- Every untrusted-media decoder runs sandboxed (`bubblewrap` + rlimits); the
  WebView web process is sandboxed and the GUI refuses to start otherwise.

### Configuration & tooling
- Single git-backed user configuration repo at `~/.config/metafolder/`,
  applied by `metafolder-sync-config` (the only git actor); no runtime fallback
  to embedded defaults.
- `Makefile` + `scripts/check-deps.sh` for build/install with dependency
  checks; `scripts/check.sh` static-analysis pass; `scripts/complete-build.sh`
  and `scripts/prune-target.sh` for a small `target/`.

### Fixed (v0.3 hardening pass)
- Documentation ↔ code drift corrected across the README, specs and roadmap
  (authentication documented and shown in examples, `follows_transitive` body
  key, `bubblewrap` promoted to a required GUI dependency, full CLI surface).
- Removed panic-prone `unwrap`/`expect` on live daemon query/reconcile paths.
- GUI panel errors are now styled as errors (were rendered as info) with
  consistent timeouts; `external_ref` renders as `repo :: metarecord` instead
  of `[object Object]`.
- De-duplicated shared helpers (index acquisition, typed field readers, hex
  encoding, file-action helpers) and cleared the standing frontend-lint errors.

### Performance
- `executor::compact` is now O(n log n) (was O(n²)), verified byte-for-byte
  against the previous implementation by a fuzz test.
