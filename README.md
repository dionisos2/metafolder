# metafolder

Metafolder is a decentralized file metadata management system. It attaches arbitrary metadata (tags, ratings, paths, notes, etc.) to files without modifying the files themselves or storing anything inside their directories. Metadata is stored in a `.metafolder/` directory at the root of each repository, similar to how `.git/` works.

File identity is hash-based, so metadata follows files when they are moved or renamed.

## Architecture

The project is a Cargo workspace with four crates:

- **`core`** — shared data structures (`Metadata`, `Field`, `Value`) and serialization logic used by all other crates.
- **`daemon`** — single background process that manages metadata for one or more repositories. Each repository can be loaded into the running daemon; it watches the filesystem for file changes (via inotify) and exposes an HTTP API for reading and writing metadata.
- **`cli`** — command-line tool that communicates with the daemon over HTTP.
- **`gui`** — keyboard-driven graphical interface (not yet implemented).

## Build

```bash
cargo build
```

## Test

```bash
cargo test
```

## Usage

### 1. Start the daemon

```bash
cargo run -p metafolder-daemon
```

By default it listens on port `7523`. Use `--port` to change it:

```bash
cargo run -p metafolder-daemon -- --port 8080
```

### 2. Use the CLI

The CLI binary is `metafolder`. Two global options control where it connects:

| Option | Env var | Default |
|--------|---------|---------|
| `--daemon-url <URL>` | `METAFOLDER_DAEMON_URL` | `http://localhost:7523` |
| `--repo <UUID>` | `METAFOLDER_REPO` | *(required for entry/query commands)* |

---

#### Repository management

```bash
# Initialize a new repository (creates .metafolder/ in the given directory)
metafolder repo init /path/to/folder
# → prints the new repo UUID

# Load an existing repository into the running daemon
metafolder repo load /path/to/folder
# → prints the repo UUID

# List all repositories currently loaded in the daemon
metafolder repo list
```

Once a repository is initialized or loaded, the daemon watches it for filesystem changes and automatically creates or updates entries.

---

#### Entry management

All entry commands require `--repo <UUID>` (or the `METAFOLDER_REPO` env var).

```bash
# List all entry UUIDs in the repository
metafolder --repo <UUID> entries list

# Get a specific entry
metafolder --repo <UUID> entries get <entry-uuid>

# Create an entry with fields (format: name:type:value, repeatable)
metafolder --repo <UUID> entries create \
  --field rating:int:5 \
  --field tag:string:jazz \
  --field note:nothing:

# Set (replace) a field on a specific entry
metafolder --repo <UUID> entries set <entry-uuid> <field> <type> <value>

# Set a field on all entries matching a query
metafolder --repo <UUID> entries set --query "tag = \"jazz\"" rating int 5

# Delete an entry
metafolder --repo <UUID> entries delete <entry-uuid>
```

Field types for `entries create` and `entries set`: `string`, `int`, `float`, `bool`, `nothing`, `ref`.

---

#### Querying

```bash
metafolder --repo <UUID> query "<predicate>"
```

Returns one UUID per line for every entry matching the predicate.

**Query DSL syntax:**

```
# Presence checks (three-valued: unknown = field absent, absent = field is Nothing)
path IS PRESENT
path IS ABSENT
path IS UNKNOWN

# Comparisons  (=  !=  <  <=  >  >=)
rating > 3
label = "jazz"
active = true

# Boolean combinators (AND binds tighter than OR)
rating > 3 AND path IS PRESENT
tag = "jazz" OR tag = "blues"
NOT path IS PRESENT

# Parentheses
(tag = "jazz" OR tag = "blues") AND rating >= 4

# Reference traversal: field points to an entry matching a condition
tag -> (label = "jazz")

# Transitive traversal: follow field zero or more hops
tag ->* (label = "music")
```

---

#### Reconcile

Reconcile syncs the database with the current state of the filesystem: creates entries for files that have no entry yet, and clears the `path` field (sets it to `Nothing`) for entries whose file no longer exists.

```bash
metafolder --repo <UUID> reconcile
# → prints: created: N  cleared: M
```

---

### Environment variables

```bash
export METAFOLDER_REPO=<uuid>
export METAFOLDER_DAEMON_URL=http://localhost:7523

# Then you can omit --repo and --daemon-url from every command:
metafolder entries list
metafolder query "rating > 3 AND path IS PRESENT"
```

---

## HTTP API

The daemon also exposes a direct HTTP API (used internally by the CLI).

**Check the daemon is up:**
```
GET /health
```

**List loaded repositories:**
```
GET /repos
```

**Initialize a new repository:**
```
POST /repos/init
Content-Type: application/json

{ "root": "/path/to/folder" }
```

**Load an existing repository:**
```
POST /repos/load
Content-Type: application/json

{ "root": "/path/to/folder" }
```

**Create a metadata entry manually:**
```
POST /repos/<repo_uuid>/entries
Content-Type: application/json

{
  "fields": [
    { "name": "rating", "value": { "type": "int",    "value": 5 } },
    { "name": "tag",    "value": { "type": "string", "value": "jazz" } }
  ]
}
```

**Retrieve an entry:**
```
GET /repos/<repo_uuid>/entries/<entry_uuid>
```

**Update a field on an entry:**
```
PATCH /repos/<repo_uuid>/entries/<entry_uuid>
Content-Type: application/json

{ "name": "rating", "value": { "type": "int", "value": 9 } }
```

**Delete an entry:**
```
DELETE /repos/<repo_uuid>/entries/<entry_uuid>
```

**Run a query (returns matching UUIDs):**
```
POST /repos/<repo_uuid>/query
Content-Type: application/json

{ "op": "IsPresent", "field": "path" }
```

**Reconcile filesystem with database:**
```
POST /repos/<repo_uuid>/reconcile
```
