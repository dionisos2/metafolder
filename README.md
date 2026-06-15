# metafolder

Metafolder is a decentralized file metadata management system. It attaches arbitrary metadata (tags, ratings, paths, notes, etc.) to files without modifying the files themselves or storing anything inside their directories. Metadata is stored in a `.metafolder/` directory at the root of each repository, similar to how `.git/` works.

File identity is hash-based, so metadata follows files when they are moved or renamed.

## Architecture

The project is a Cargo workspace:

- **`core`** — shared data model (`Metadata`, `Field`, `Value`, `Query`), its JSON serialization, and the query DSL parser.
- **`daemon`** — single background process managing one or more repositories: SQLite storage with a full event log, filesystem watcher (inotify), on-demand reconcile with fingerprint matching, query engine, user schema validation, and an HTTP API.
- **`cli`** — the `mf` command-line client: a thin client over the daemon's HTTP API (repository management, entry CRUD, query DSL, reconcile/track, schema commands).
- **`gui`** — the `metafolder-gui` desktop app (Tauri v2 + Svelte 5): workspaces (tabs) with two panel slots, a keybinding system, a command input with autocomplete, and a local scripting HTTP API (`/gui/*`). Panel types are plain HTML/JS directories rendered in iframes and can be customized or added by the user.
- **`bench`** — benchmarking harness (targets the old POC API; to be updated with the CLI).

The full specification lives under `docs/` (`spec-*.org`).

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

## Running the GUI

Building the gui crate needs the Tauri system libraries (Arch:
`webkit2gtk-4.1 gtk3 librsvg`) and `npm`. Build the frontend first, then run
`metafolder-gui` with the daemon running:

```bash
npm --prefix crates/gui/frontend install   # once
npm --prefix crates/gui/frontend run build

cargo run -p metafolder-gui                # GUI API on 127.0.0.1:7524
cargo run -p metafolder-gui -- --gui-port 7524 --daemon-url http://127.0.0.1:7523
```

On first run the GUI installs its editable configuration under
`~/.config/metafolder-gui/` (`keybindings.toml`, `style.css`, and the
built-in panel types as user-copyable HTML/JS); user edits are never
overwritten. The local HTTP API on port 7524 lets external scripts drive the
GUI (workspaces, layout, panel views, messages, input prompts). See
`docs/spec-gui.org`.

## Quick tour (mf)

With the daemon running:

```bash
# Initialise a repository (creates .metafolder/ inside the directory)
REPO=$(cargo run -p metafolder-cli -- init /path/to/folder)
export METAFOLDER_REPO=$REPO          # or pass --repo $REPO to each command

# Nothing is tracked by default (opt-in). Enable tracking on the root entry:
ROOT=$(mf query 'mf_watch IS PRESENT')
mf set $ROOT mf_watch:bool=true

# Index the files already present (the watcher tracks new changes live):
mf reconcile

# Query: every file under /music, with sizes
mf query 'mfr_path ->* "/music"' --select mfr_size

# Tag a file, then find it back
mf set $(mf query 'mfr_path ->* "/music"' --limit 1) genre:string=jazz
mf query 'genre = "jazz"'
```

`mf --help` lists all commands (`init`, `load`, `repos`, `list`, `get`,
`create`, `set`, `add`, `unset`, `delete`, `query`, `reconcile`, `track`,
`path`, `schema check|reload|show`, and the `gui` subcommands below).
Global options: `--daemon-url` (`METAFOLDER_DAEMON_URL`) and `--repo`
(`METAFOLDER_REPO`). Exit codes: 0 success, 1 operation failed, 2 usage
error.

`mf gui …` drives a running GUI through its scripting API (`status`,
`repo`, `workspace new|rm`, `layout`, `view`, `message`, `input`,
`prompt`); see `scripts/gui-tag-pair.sh` for a complete interactive
example (per-file yes/no tagging with prompt autocompletion).

## Quick tour (curl)

```bash
B=localhost:7523

# Initialise a repository (creates .metafolder/ inside the directory)
REPO=$(curl -s -X POST $B/repos/init -d '{"root": "/path/to/folder"}' \
       -H 'content-type: application/json' | jq -r .repo_uuid)

# Nothing is tracked by default (opt-in). Enable tracking on the root entry:
ROOT=$(curl -s -X POST $B/repos/$REPO/query \
       -d '{"query": {"type": "is_present", "field": "mf_watch"}}' \
       -H 'content-type: application/json' | jq -r '.[0]')
curl -s -X PATCH $B/repos/$REPO/metadata/$ROOT \
     -d '{"name": "mf_watch", "value": {"type": "bool", "value": true}}' \
     -H 'content-type: application/json'

# Index the files already present (the watcher tracks new changes live):
curl -s -X POST $B/repos/$REPO/reconcile

# Query: every file under /music, with sizes
curl -s -X POST $B/repos/$REPO/query -H 'content-type: application/json' -d '{
  "query": {"type": "follows_transitive", "field": "mfr_path", "path": "/music"},
  "select": ["mfr_size"]
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
| `GET /health` | Liveness check |
| `GET /repos`, `POST /repos/init`, `POST /repos/load` | Repository management (`init`/`load` accept an external `metafolder` location) |
| `GET\|POST /repos/:repo/metadata` | List (paginated with `?limit&cursor`) / create entries |
| `GET\|PATCH\|DELETE /repos/:repo/metadata/:uuid` | Read / set-field / delete one entry |
| `POST .../metadata/:uuid/fields`, `PUT\|DELETE .../fields/:field_id` | Multi-map field operations |
| `POST /repos/:repo/query` | Query engine (`select`, `sort`, keyset pagination) |
| `POST /repos/:repo/set` | Batch set on every query match (one transaction) |
| `POST /repos/:repo/reconcile` | Full reconcile (fingerprint phase, candidates) |
| `POST /repos/:repo/track` | Track a single path without activating the watch scope |
| `POST .../metadata/:uuid/reconcile` | Reconcile one subtree |
| `GET /repos/:repo/schema`, `POST .../schema/reload`, `POST .../schema/check` | User schema (spec-schema) |
| `GET /repos/:repo/log`, `GET\|PATCH .../log/revisions/:rev_id` | Event log reading, labels |
| `POST /repos/:repo/rollback`, `POST /repos/:repo/log/prune` | Atomic navigation, pruning |

Key concepts (see `docs/spec-data-model.org`):

- Everything is a metadata entry: a multi-map of `(name, value)` fields with
  ten value types, including `tree_ref` (a position in a named tree — the
  filesystem tree uses the reserved `mfr_path` field).
- Three-valued logic: a field can be *present*, explicitly *absent*
  (`nothing`), or *unknown* (no row).
- `mfr_*` fields are daemon-owned (require `"force": true` to override);
  `mf_*` fields (`mf_watch`, `mf_ignore`, `mf_schema`) control the daemon.
- Tracking is opt-in per subtree via `mf_watch`/`mf_ignore` inheritance.
- Every write is recorded in an event log; any past state can be restored
  with `POST /rollback`, and new writes after a rollback create branches.
