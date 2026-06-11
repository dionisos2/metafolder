# metafolder

Metafolder is a decentralized file metadata management system. It attaches arbitrary metadata (tags, ratings, paths, notes, etc.) to files without modifying the files themselves or storing anything inside their directories. Metadata is stored in a `.metafolder/` directory at the root of each repository, similar to how `.git/` works.

File identity is hash-based, so metadata follows files when they are moved or renamed.

## Architecture

The project is a Cargo workspace:

- **`core`** — shared data model (`Metadata`, `Field`, `Value`, `Query`) and its JSON serialization.
- **`daemon`** — single background process managing one or more repositories: SQLite storage with a full event log, filesystem watcher (inotify), on-demand reconcile with fingerprint matching, query engine, user schema validation, and an HTTP API.
- **`cli`** — stub; the v1 CLI (`mf`) is the next roadmap item.
- **`gui`** — stub, not yet implemented.
- **`bench`** — benchmarking harness (targets the old POC API; to be updated with the CLI).

The full specification lives under `docs/` (`spec-*.org`).

## Build and test

```bash
cargo build
cargo test
```

## Running the daemon

```bash
cargo run -p metafolder-daemon            # listens on 127.0.0.1:7523
cargo run -p metafolder-daemon -- --port 8080
```

The daemon is local-only and unauthenticated; access control is left to the OS.

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
