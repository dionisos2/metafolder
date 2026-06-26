# metafolder

Metafolder is a file metadata management system. It attaches arbitrary metadata (tags, ratings, paths, notes, etc.) to files without modifying the files themselves or storing anything inside their directories. Metadata is stored in a `.metafolder/` directory at the root of each repository, similar to how `.git/` works.

File identity is hash-based, so metadata follows files when they are moved or renamed.

> **Status: v0.1 — nothing is stable yet.** This is early development:
> APIs, the CLI command tree, the on-disk database layout and the config
> format all change without migrations or deprecation. Do not rely on it for
> anything you cannot afford to lose. This project has been built
> predominantly with [Claude Code](https://claude.com/claude-code).

## Architecture

The project is a Cargo workspace:

- **`core`** — shared data model (`MetaRecord`, `Field`, `Value`, `Query`), its JSON serialization, the query DSL parser, and the simplified-query language (a user-editable grammar that expands client-side into the normal DSL).
- **`daemon`** — single background process managing one or more repositories: SQLite storage (EAV schema + FTS5 trigram index) with a full event log, filesystem watcher (inotify), on-demand reconcile with fingerprint matching, an in-memory tree cache and bitmap/BSI query accelerator, user schema validation, and an HTTP API.
- **`cli`** — the `mf` command-line client: a thin client over the daemon's HTTP API (repository management, metarecord CRUD, query DSL, reconcile/track, log/rollback/prune, schema, and GUI scripting).
- **`gui`** — the `metafolder-gui` desktop app (Tauri v2 + Svelte 5): workspaces (tabs) with two panel slots, a keybinding system, a command input with autocomplete, and a local scripting HTTP API (`/gui/*`). Panel types are plain HTML/JS directories mounted in Shadow DOM roots (no iframes) and can be customized or added by the user.
- **`bench`** — benchmarking harness running against two persistent data folders (`data`, `gui`, `attach` modes).

The full specification lives under `docs/` (`spec-*.org`).

## Configuration

The daemon and GUI read their configuration from `~/.config/metafolder/<crate>/`
(`core/`, `daemon/`, `gui/`, `cli/`). There is **no runtime fallback to embedded
defaults**: a missing or invalid config file is a hard startup error, so the
config repository must be installed first:

```bash
# Install/update the user config repo at ~/.config/metafolder/ (git-backed)
cargo run -p metafolder-core --features sync-config --bin metafolder-sync-config
```

`metafolder-sync-config` is the only git actor: it gathers each crate's
`default-config/`, keeps them on a `default` branch (shipped defaults), and
3-way-merges them into `main` (your edits) without clobbering your changes.
Re-run it after pulling new built-in panels or default keybindings. See
`docs/spec-config.org`.

## Build and test

```bash
cargo build
cargo test

# GUI frontend (vitest): npm install once in crates/gui/frontend
npm --prefix crates/gui/frontend test
```

## Running the daemon

```bash
cargo run -p metafolder-daemon            # listens on 127.0.0.1:7523
cargo run -p metafolder-daemon -- --port 8080
```

The daemon is local-only and unauthenticated; access control is left to the OS.
Repositories listed in `~/.config/metafolder/daemon/config.toml` are auto-loaded
at startup.

## Running the GUI

Building the gui crate needs the Tauri system libraries (Arch:
`webkit2gtk-4.1 gtk3 librsvg`) and `npm`. Build the frontend first, then run
`metafolder-gui` with the daemon running:

```bash
npm --prefix crates/gui/frontend install   # once
npm --prefix crates/gui/frontend run build

cargo run -p metafolder-gui                              # GUI API on 127.0.0.1:7524
cargo run -p metafolder-gui -- --gui-port 7524 --daemon-port 7523
```

Ports come from `~/.config/metafolder/gui/config.toml` (`gui-port` default 7524,
`daemon-port` default 7523); the CLI flags above override them. The GUI never
installs or upgrades anything at startup — its keybindings, stylesheet and panel
types come from the config repo (run `metafolder-sync-config` first). The local
HTTP API on the GUI port lets external scripts drive the GUI (workspaces,
layout, panel views, messages, input prompts). See `docs/spec-gui.org`.

## Quick tour (mf)

With the daemon running:

```bash
# Initialise a repository (creates .metafolder/ inside the directory)
REPO=$(mf repo init /path/to/folder)
export METAFOLDER_REPO=$REPO          # or pass -u $REPO to each command

# Nothing is tracked by default (opt-in). Enable tracking on the root metarecord:
ROOT=$(mf metarecord -q 'mf_watch IS PRESENT' get)
mf metarecord -i $ROOT field set mf_watch:bool=true

# Index the files already present (the watcher tracks new changes live):
mf reconcile

# Query: every file under /music, with sizes
mf metarecord -q 'mfr_path ->* "/music"' get --select mfr_size

# Tag one file, then find it back
FILE=$(mf metarecord -q 'mfr_path ->* "/music"' get --limit 1)
mf metarecord -i $FILE field set genre:string=jazz
mf metarecord -q 'genre = "jazz"' get
```

### CLI command tree

Repository and daemon are selected once by **global flags that precede the
command**: `-n <name>` / `-u <uuid>` (repository, mutually exclusive) and
`-p <port>` (daemon, default 7523). Env fallbacks: `METAFOLDER_REPO_NAME`,
`METAFOLDER_REPO`, `METAFOLDER_DAEMON_PORT`. Exit codes: 0 success, 1 operation
failed, 2 usage error.

The command tree is a noun/verb hierarchy (`get`⁻¹`set`, `add`⁻¹`delete`):

- `mf repo {list,init,load,unload}` — repository lifecycle.
- `mf metarecord [selector] <verb>` — the selector is an **option before the
  verb**: `-q "<DSL>"` (query; add `-s` to write it in the simplified
  language), `-i <uuid>` (one metarecord), or none (all). Verbs: `get`,
  `add <specs>`, `set <specs>`, `delete`, and `field <get|set|add|delete|unset>
  <name|spec>`.
- `mf field {list,get,set,delete}` — `list [--type <value_type>]` enumerates the
  repository's distinct field names with their value type (`list` is the
  default, so `mf field` ≡ `mf field list`); `get`/`set`/`delete <id>` give
  direct field-row access by DB id.
- `mf retype <name> <type>` — convert a field's value type repository-wide.
- `mf reconcile`, `mf track <path>`, `mf path <uuid>` — filesystem sync.
- `mf log {list,show,rollback,prune}` — event log, atomic navigation, pruning.
- `mf task {list,show}` — background tasks.
- `mf schema {check,reload,show}` — user schema.
- `mf gui …` — drive a running GUI through its scripting API (`status`, `repo`,
  `workspace new|rm`, `layout`, `view`, `message`, `input`, `prompt`); see
  `scripts/gui-tag-pair.sh` for a complete interactive example.

Field specs are `name:type[=value]` (e.g. `genre:string=jazz`, `rating:int=5`).
Run `mf --help` (and `mf <command> --help`) for the full set of options.

## Quick tour (curl)

The HTTP API has two layers (see `docs/spec-data-model.org` /
`docs/spec-query.org`): a **resource layer** for a single directly-addressed
thing (`…/metarecords/:uuid`, `…/fields/:name`, `…/fields/:id`) and a **set
layer** (`POST …/query/*`) where every body carries a `query`.

```bash
B=localhost:7523

# Initialise a repository (creates .metafolder/ inside the directory)
REPO=$(curl -s -X POST $B/repos/init -d '{"root": "/path/to/folder"}' \
       -H 'content-type: application/json' | jq -r .repo_uuid)

# Nothing is tracked by default (opt-in). Enable tracking on the root metarecord:
ROOT=$(curl -s -X POST $B/repos/$REPO/query \
       -d '{"query": {"type": "is_present", "field": "mf_watch"}}' \
       -H 'content-type: application/json' | jq -r '.[0]')
curl -s -X PUT $B/repos/$REPO/metarecords/$ROOT/fields/mf_watch \
     -d '{"value": {"type": "bool", "value": true}}' \
     -H 'content-type: application/json'

# Index the files already present (the watcher tracks new changes live):
curl -s -X POST $B/repos/$REPO/reconcile

# Query: every file under /music, with sizes
curl -s -X POST $B/repos/$REPO/query -H 'content-type: application/json' -d '{
  "query": {"type": "follows_transitive", "field": "mfr_path", "path": "/music"},
  "select": ["mfr_size"]
}'

# Set a field on every match of a query (one transaction)
curl -s -X POST $B/repos/$REPO/query/fields/set -H 'content-type: application/json' -d '{
  "query": {"type": "eq", "field": "genre", "value": {"type": "string", "value": "jazz"}},
  "name": "reviewed",
  "value": {"type": "bool", "value": true}
}'

# Undo the last revision (metadata only)
curl -s -X POST $B/repos/$REPO/rollback \
     -d '{"target": {"prev_revision": true}}' -H 'content-type: application/json'
```

## HTTP API overview

All bodies are JSON; errors are `{"error": "<message>"}` with a meaningful
status code. UUIDs are 32-char lowercase hex strings. See the `docs/` specs
for the full request/response formats.

| Route | Description |
|---|---|
| `GET /health`, `GET /tasks` | Liveness check / all background tasks |
| `GET /repos`, `POST /repos/init`, `POST /repos/load` | Repository management (`init`/`load` accept an external `metafolder` location) |
| `GET\|PATCH /repos/:repo`, `POST .../unload` | Repo info / rename / unload |
| `POST /repos/:repo/metarecords` | Create a metarecord |
| `GET\|PUT\|DELETE .../metarecords/:uuid` | Read / overwrite / delete one metarecord |
| `POST .../metarecords/:uuid/fields` | Append a field (multi-map) |
| `GET\|PUT\|DELETE .../metarecords/:uuid/fields/:name` | Read / set / unset a field by name |
| `GET .../metarecords/:uuid/fields/:name/resolve-tree` | Resolve a `tree_ref` field to its path |
| `GET\|PATCH\|DELETE .../fields/:id` | Direct field-row access by DB id |
| `GET /repos/:repo/fields[?type=]` | Distinct field names + value types (optionally filtered) |
| `POST /repos/:repo/retype` | Convert a field's value type repository-wide |
| `POST /repos/:repo/query` | Query engine (`select`, `sort`, keyset pagination) |
| `POST .../query/delete` | Delete every match (one transaction) |
| `POST .../query/fields/{set,append,remove,unset}` | Batch field ops over every match |
| `POST .../query/fields/resolve-tree` | Resolve a `tree_ref` field for a query's matches |
| `POST /repos/:repo/reconcile`, `POST .../track` | Full reconcile / track a single path |
| `GET /repos/:repo/log`, `GET .../log/since` | Event-log reading / change feed |
| `GET\|PATCH .../log/revisions/:rev_id`, `POST .../log/prune` | Revision labels / pruning |
| `POST /repos/:repo/rollback` (+ `/plan`, `/start`, `/step`, `/abort`) | Atomic & coordinated navigation |
| `GET /repos/:repo/schema`, `POST .../schema/{reload,check}` | User schema (spec-schema) |
| `GET /repos/:repo/tasks`, `GET\|POST .../tasks/:task[/cancel]` | Background tasks |

Key concepts (see `docs/spec-data-model.org`):

- Everything is a **metarecord**: a multi-map of `(name, value)` fields with
  ten value types, including `tree_ref` (a position in a named tree — the
  filesystem tree uses the reserved `mfr_path` field).
- Three-valued logic: a field can be *present*, explicitly *absent*
  (`nothing`), or *unknown* (no row).
- `mfr_*` fields are daemon-owned (require `"force": true` to override);
  `mf_*` fields (`mf_watch`, `mf_ignore`, `mf_schema`) control the daemon.
- Tracking is opt-in per subtree via `mf_watch`/`mf_ignore` inheritance.
- Every write is recorded in an event log; any past state can be restored
  with `POST /rollback`, and new writes after a rollback create branches.
