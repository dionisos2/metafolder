# metafolder

Metafolder is a decentralized file metadata management system. It attaches arbitrary metadata (tags, ratings, paths, notes, etc.) to files without modifying the files themselves or storing anything inside their directories. Metadata is stored in a `.metafolder/` directory at the root of each repository, similar to how `.git/` works.

File identity is hash-based, so metadata follows files when they are moved or renamed.

## Architecture

The project is a Cargo workspace with four crates:

- **`core`** — shared data structures (`Metadata`, `Field`, `Value`) and serialization logic used by all other crates.
- **`daemon`** — single background process that manages metadata for one or more repositories. Each repository can be loaded into the running daemon; it watches the filesystem for file changes (via inotify) and exposes an HTTP API for reading and writing metadata.
- **`cli`** — command-line tool (not yet implemented) that communicates with the daemon.
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

Start the daemon on a directory:

```bash
cargo run -p metafolder-daemon -- --root /path/to/your/folder
```

By default it listens on port `7523`. Use `--port` to change it:

```bash
cargo run -p metafolder-daemon -- --root /path/to/your/folder --port 8080
```

Once running, the daemon automatically creates an entry in the database for each file that appears in the directory, and removes it when the file is deleted.

### HTTP API

**Check the daemon is up:**
```
GET /health
```

**Create a metadata entry manually:**
```
POST /entries
Content-Type: application/json

{
  "db_id": "<uuid>",
  "fields": [
    { "name": "rating", "value": { "Int": 5 } },
    { "name": "tag",    "value": { "String": "jazz" } }
  ]
}
```

**Retrieve an entry:**
```
GET /entries/<uuid>
```
