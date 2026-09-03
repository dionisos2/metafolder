//! `mf` binary: argument parsing (clap) and dispatch to
//! [`metafolder_cli::commands`]. Exit codes (spec-main): 0 success,
//! 1 operation failed, 2 usage error (clap also exits 2 on bad arguments).
//!
//! The command tree mirrors the data model (spec-data-model "* CLI"): a verb
//! quartet (`get`/`set`/`add`/`delete`) at the metarecord, field-name and
//! field-id levels, plus the `repo`/`task`/`log` management groups. Repository
//! and daemon are selected once by the global `-n`/`-u`/`-p` flags.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use metafolder_cli::commands::{self, Ctx};
use metafolder_cli::gui::{self, GuiCtx};
use metafolder_cli::log;
use metafolder_cli::sync;

#[derive(Parser)]
#[command(name = "mf", about = "metafolder CLI — thin client over the daemon HTTP API")]
struct Cli {
    /// Target repository by (unique) name
    #[arg(short = 'n', long = "name", env = "METAFOLDER_REPO_NAME")]
    repo_name: Option<String>,

    /// Target repository by UUID
    #[arg(short = 'u', long = "uuid", env = "METAFOLDER_REPO")]
    repo_uuid: Option<String>,

    /// Daemon port on 127.0.0.1 (default: `daemon-port` from the config, else 7523)
    #[arg(short = 'p', long = "port", env = "METAFOLDER_DAEMON_PORT")]
    port: Option<u16>,

    /// Ignore the config file entirely (use built-in defaults) — for scripts
    #[arg(long = "no-config")]
    no_config: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Repository management (default: list)
    Repo {
        #[command(subcommand)]
        command: Option<RepoCommand>,
    },
    /// Background tasks (default: list)
    Task {
        #[command(subcommand)]
        command: Option<TaskCommand>,
    },
    /// Revision/operation history (default: list)
    Log {
        #[command(subcommand)]
        command: Option<LogCommand>,
    },
    /// Trash-bin: safely trash a file, or restore/prune (default: list)
    Trash {
        /// Move a tracked file into the trash (errors if it has no metarecord)
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        file: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<TrashCommand>,
    },
    /// Orphans: list metarecords whose file is gone, or clear them (default: list)
    ///
    /// An orphan is a tracked metarecord whose `mfr_path` still resolves to a
    /// path, but whose file no longer exists on disk (deleted while unwatched,
    /// or left stale by reconcile). Detection needs a disk scan, so this is a
    /// distinct command rather than a query. `clear` orphans them properly
    /// (freezing `mfr_path_old`, setting `mfr_path` to Nothing).
    Orphan {
        #[command(subcommand)]
        command: Option<OrphanCommand>,
    },
    /// Mount points: list the repository's removable volumes, or forget one
    ///
    /// A directory that is a mount point is marked with `mfr_mount` by the
    /// daemon. While nothing is mounted there its subtree is *frozen* — not
    /// walked, not orphaned, not watched — so the metadata of an unplugged
    /// drive survives untouched. `forget` un-declares a directory that is not
    /// a mount point any more.
    Mount {
        #[command(subcommand)]
        command: Option<MountCommand>,
    },
    /// Watcher ingestion: show, pause or resume the recording of filesystem
    /// events
    ///
    /// A flush can be long — a directory of a hundred thousand files arrives as
    /// one batch — and while it runs the repository answers nothing else.
    /// `pause` stops the flush in progress and keeps the executor from starting
    /// another; the events stay buffered, and `resume` applies them.
    Watch {
        #[command(subcommand)]
        command: Option<WatchCommand>,
    },
    /// Metarecord operations: `mf metarecord [selector] <verb>`
    ///
    /// The selector picks the target metarecord(s) and precedes the verb:
    /// `-q "<DSL query>"` (add `-s` for the simplified language), `-i <uuid>`
    /// for a single metarecord, or none to mean all. Examples:
    ///   mf metarecord get                         # every uuid
    ///   mf metarecord -i <uuid> get               # one metarecord, full JSON
    ///   mf metarecord -q 'rating > 3' get         # matching uuids
    ///   mf metarecord -q 'mfr_path ->* "/docs"' field set reviewed:bool=true
    #[command(verbatim_doc_comment)]
    Metarecord {
        /// DSL query selector (use -s to write it in the simplified language)
        #[arg(short = 'q', long = "query")]
        query: Option<String>,
        /// Single-metarecord selector by UUID
        #[arg(short = 'i', long = "id", conflicts_with = "query")]
        id: Option<String>,
        /// Exact-match selector: name[:type]=value (repeatable, AND-ed; safe —
        /// no DSL interpolation). Mutually exclusive with -q/-i.
        #[arg(long = "eq", conflicts_with_all = ["query", "id"])]
        eq: Vec<String>,
        /// Treat -q as simplified-language text and expand it first
        #[arg(short = 's', long = "simplified", requires = "query")]
        simplified: bool,
        #[command(subcommand)]
        verb: Option<MetarecordVerb>,
    },
    /// Field operations: list the repo's field names, or access a row by id
    Field {
        #[command(subcommand)]
        command: Option<FieldCommand>,
    },
    /// Convert a field's value type repository-wide (string|int|float|bool|datetime)
    Retype {
        /// Field name
        name: String,
        /// Target type: string, int, float, bool, or datetime
        to: String,
    },
    /// Reconcile the database with the filesystem
    Reconcile {
        /// Single-metarecord reconcile, scoped to this metarecord's subtree
        #[arg(long)]
        metarecord: Option<String>,
        /// Enable the similarity phase with this minimum score, range [0, 1]
        #[arg(long)]
        threshold: Option<f64>,
        /// Do not compute mfr_mime for files that lack it
        #[arg(long = "no-mime")]
        no_mime: bool,
        /// Do not extract embedded mfr_meta_* fields for files not yet analysed
        #[arg(long = "no-metadata")]
        no_metadata: bool,
        /// Do not refresh mfr_* stat fields of in-place (unmoved) files
        #[arg(long = "no-refresh")]
        no_refresh: bool,
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
        /// Start the (full) reconcile and print its task id without waiting
        #[arg(long = "no-wait")]
        no_wait: bool,
        /// Poll interval in ms while waiting (default: reconcile-poll-interval-ms from config, else 200)
        #[arg(long = "poll-interval")]
        poll_interval: Option<u64>,
    },
    /// Create the metarecord for a single path and print its UUID
    Track { path: PathBuf },
    /// Print the filesystem path of a metarecord (walks the mfr_path chain)
    Path {
        uuid: String,
        /// Print the path relative to the repository root
        #[arg(long)]
        relative: bool,
    },
    /// Number a folder's direct children for sorting: assigns
    /// `order_position_file` / `order_position_dir` (files and dirs are numbered
    /// independently; an existing position is never overwritten). Orders by an
    /// ordering metadata, then a shared name pattern, then creation date.
    Order {
        /// The folder whose direct children to number
        path: PathBuf,
        /// Metadata field giving an explicit order
        #[arg(long, default_value = "mfr_meta_track")]
        meta: String,
        /// Largest gap between shared-pattern name numbers still treated as an
        /// order (a larger gap ⇒ the number is an id/hash, ignored)
        #[arg(long = "max-gap", default_value_t = metafolder_cli::order::DEFAULT_MAX_GAP)]
        max_gap: i64,
        /// Print the planned positions without writing them
        #[arg(long)]
        dry_run: bool,
    },
    /// Hierarchical tags: add/deny/mixed/remove a tag on the selected
    /// metarecord(s), or list the vocabulary. Encapsulates the subsumption and
    /// exclusivity rules (drop ancestors on add, descendants on deny). The
    /// field-name convention is configurable via the `[tag]` config table.
    Tag {
        /// DSL query selector (use -s for the simplified language)
        #[arg(short = 'q', long = "query")]
        query: Option<String>,
        /// Single-metarecord selector by UUID
        #[arg(short = 'i', long = "id", conflicts_with = "query")]
        id: Option<String>,
        /// Exact-match selector: name[:type]=value (repeatable, AND-ed)
        #[arg(long = "eq", conflicts_with_all = ["query", "id"])]
        eq: Vec<String>,
        /// Treat -q as simplified-language text and expand it first
        #[arg(short = 's', long = "simplified", requires = "query")]
        simplified: bool,
        #[command(subcommand)]
        verb: TagVerb,
    },
    /// Named mf_ignore presets: apply/replace/remove them on a directory
    /// (default: list). The target directory is chosen with -d <path>
    /// (default: the repository root).
    Ignore {
        #[command(subcommand)]
        command: Option<IgnoreCommand>,
    },
    /// User schema commands
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Drive the GUI through its scripting HTTP API
    Gui {
        /// GUI base URL (default: gui-port from the GUI config.toml)
        #[arg(long, env = "METAFOLDER_GUI_URL")]
        gui_url: Option<String>,
        #[command(subcommand)]
        command: GuiCommand,
    },
    /// Cross-repo synchronisation (spec-sync). Repos are named positionally;
    /// their order does not matter (roles are canonical).
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
}

#[derive(Subcommand)]
enum RepoCommand {
    /// List the loaded repositories (pretty-printed JSON)
    List {
        /// Also list daemon-internal system repos (e.g. sync plan repos)
        #[arg(long)]
        all: bool,
    },
    /// Initialise a new repository and print its UUID
    ///
    /// Applies the `default` ignore preset to the new root (spec-file-tracking
    /// "Ignore presets"); use --ignore to pick a different preset set, or
    /// --no-ignore to leave the root's mf_ignore empty.
    Init {
        root: PathBuf,
        /// External database location (instead of <root>/.metafolder)
        #[arg(long)]
        metafolder: Option<PathBuf>,
        /// Ignore presets to apply to the new root (comma/space separated;
        /// default: "default")
        #[arg(long = "ignore", value_name = "PRESETS", conflicts_with = "no_ignore")]
        ignore: Vec<String>,
        /// Leave the new root's mf_ignore set empty
        #[arg(long = "no-ignore")]
        no_ignore: bool,
    },
    /// Load an existing repository, wait for its warmup and print its UUID
    Load {
        root: Option<PathBuf>,
        /// Load from an external .metafolder directory
        #[arg(long)]
        metafolder: Option<PathBuf>,
        /// Print the UUID immediately, without waiting for the warmup
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
    /// Unload the selected repository (stops its watcher, releases its DB lock)
    Unload,
}

/// Shared arguments for the `add`/`remove`/`set` ignore verbs.
#[derive(clap::Args)]
struct IgnoreArgs {
    /// Preset names (comma- or space-separated), e.g. `rust-build,node`.
    /// May be empty for `set` (which then clears the target's mf_ignore).
    #[arg(value_name = "PRESETS")]
    presets: Vec<String>,
    /// Target directory (default: the repository root)
    #[arg(short = 'd', long = "dir", value_name = "PATH")]
    dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum IgnoreCommand {
    /// Append the presets' patterns to the target's mf_ignore set
    Add(IgnoreArgs),
    /// Remove the presets' patterns from the target's mf_ignore set
    Remove(IgnoreArgs),
    /// Replace the target's whole mf_ignore set with the presets' patterns
    /// (with no preset, clears the set)
    Set(IgnoreArgs),
    /// List the available presets (with -d, also the target's active patterns)
    List {
        /// Target directory whose active patterns to also print
        #[arg(short = 'd', long = "dir", value_name = "PATH")]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// List background tasks
    List {
        /// List tasks across all loaded repositories (no repo selector needed)
        #[arg(long)]
        all: bool,
        /// Print the raw JSON array
        #[arg(long)]
        json: bool,
    },
    /// Show a single background task by id (or stop it with --stop)
    Show {
        id: String,
        /// Request cancellation of the task instead of showing it (spec-tasks).
        #[arg(long, alias = "cancel")]
        stop: bool,
        /// Print the raw JSON object
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum LogCommand {
    /// Display the revision/operation history
    List {
        /// Show all branches as a flat list, not just the active line
        #[arg(long)]
        tree: bool,
        /// Draw every branch as an ASCII graph
        #[arg(long)]
        graph: bool,
        /// Expand each revision to show its individual operations
        #[arg(long)]
        ops: bool,
        /// Only revisions/ops that affected this metarecord
        #[arg(long)]
        metarecord: Option<String>,
        /// Show at most N revisions (or operations with --ops); default 20
        #[arg(long = "limit")]
        limit: Option<usize>,
        /// Only revisions with timestamp ≥ T (ISO-8601, or @<unix-ms>)
        #[arg(long)]
        since: Option<String>,
        /// Only revisions with timestamp ≤ T (ISO-8601, or @<unix-ms>)
        #[arg(long)]
        until: Option<String>,
        /// Remove the default limit of 20
        #[arg(long)]
        all: bool,
    },
    /// Show full details of one revision (a revision id, or HEAD)
    Show {
        target: String,
        /// Print the raw JSON response body
        #[arg(long)]
        raw: bool,
    },
    /// Navigate the history with coordinated filesystem moves
    Rollback {
        /// "plan" to preview, optionally a target label; or a target label.
        #[arg(num_args = 0..=2)]
        args: Vec<String>,
        /// Target operation by id
        #[arg(long)]
        id: Option<i64>,
        /// Target by revision timestamp (ISO-8601, or @<unix-ms>)
        #[arg(long)]
        timestamp: Option<String>,
        /// Policy when the file is present: apply|skip|abort|ask (default apply)
        #[arg(long = "on-move-available")]
        on_move_available: Option<String>,
        /// Policy when the file is missing: apply|skip|abort|ask (default ask)
        #[arg(long = "on-move-unavailable")]
        on_move_unavailable: Option<String>,
        /// Suppress informational output (still prompts for ask)
        #[arg(long)]
        silent: bool,
    },
    /// Permanently remove operations from the history (irreversible)
    Prune {
        #[command(subcommand)]
        command: PruneCommand,
    },
}

#[derive(Subcommand)]
enum TagVerb {
    /// The record(s) have the tag (drops more general ancestor tags)
    Add { path: String },
    /// The record(s) do NOT have the tag (drops more specific descendants)
    Deny { path: String },
    /// Mark the folder(s) mixed w.r.t. the tag
    Mixed { path: String },
    /// Drop the tag from the record(s) (undo of add)
    Remove { path: String },
    /// List the tag vocabulary as TSV: name<TAB>partition<TAB>exclusive
    List,
}

#[derive(Subcommand)]
enum MetarecordVerb {
    /// Read: -i <uuid> → full JSON; -q/none → UUIDs (--select for fields)
    Get {
        /// Print full metadata restricted to these fields, or '*' for all
        #[arg(long)]
        select: Option<String>,
        /// Sort key field[:asc|desc]; repeatable (query selectors only)
        #[arg(long = "sort")]
        sort: Vec<String>,
        /// Stop after N metarecords
        #[arg(long)]
        limit: Option<usize>,
        /// Print the selected field's raw values, one per line
        #[arg(long, requires = "select")]
        values: bool,
        /// Print one tab-separated row per metarecord (first value of each
        /// --select field). Requires --select with a field list.
        #[arg(long, requires = "select", conflicts_with = "values")]
        tsv: bool,
        /// Resolve this tree_ref field of each selected metarecord to its
        /// root-relative path(s), one per line (the bulk form of `mf path`).
        /// Needs a selector (-q or -i).
        #[arg(long = "resolve-tree", conflicts_with_all = ["select", "values", "tsv", "sort"])]
        resolve_tree: Option<String>,
    },
    /// Create a metarecord with the given fields and print its UUID (no selector)
    Add {
        /// Field spec name:type[=value]; repeatable
        #[arg(required = true)]
        specs: Vec<String>,
        /// Required to write mfr_* fields
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Replace the ENTIRE field set of the selected metarecord (needs -i, force)
    Set {
        /// Field spec name:type[=value]; repeatable
        #[arg(required = true)]
        specs: Vec<String>,
        /// Mandatory: confirms the full-record overwrite
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Delete the selected metarecords (metadata and all fields)
    Delete {
        /// Skip the confirmation prompt for query selectors
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Field operations scoped to the selected metarecord(s)
    Field {
        #[command(subcommand)]
        verb: FieldVerb,
    },
}

#[derive(Subcommand)]
enum FieldVerb {
    /// Print the field's value(s)
    Get {
        name: String,
        /// Treat each value as a Ref and print the referenced records' <field>
        /// instead (one round-trip). Requires -i.
        #[arg(long)]
        resolve: Option<String>,
    },
    /// Replace all rows of the field with the given value(s)
    Set {
        /// Field spec name:type[=value]; repeatable (multi-map set)
        #[arg(required = true)]
        specs: Vec<String>,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Append one row (inverse of delete)
    Add {
        /// Field spec name:type[=value]
        spec: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Remove the row(s) equal to the spec (inverse of add)
    Delete {
        /// Field spec name:type[=value]
        spec: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Remove the field entirely (every row → unknown)
    Unset {
        name: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Subcommand)]
enum FieldCommand {
    /// List the distinct field names of the repository (with their value type)
    List {
        /// Restrict to one value type (e.g. tree_ref, ref, string, int)
        #[arg(long = "type")]
        type_filter: Option<String>,
    },
    /// Print a field row by its id
    Get { id: i64 },
    /// Change a row's name and/or value in place, keeping its id
    Set {
        id: i64,
        /// Field spec name:type[=value] — the new name and value
        spec: String,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Delete a field row by its id
    Delete {
        id: i64,
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Subcommand)]
enum TrashCommand {
    /// List the trashed files (id, size, age, reason, original path)
    List,
    /// Restore a trashed file to its original path
    Restore {
        /// The trash entry id (from `mf trash list`)
        id: String,
    },
    /// Permanently delete trashed files (by size, age, or all)
    Prune {
        /// Delete oldest-first until the total is under this size (e.g. 100mb, 1g)
        #[arg(short = 's', long = "size")]
        size: Option<String>,
        /// Delete entries older than this duration (e.g. 1y, 30d, 12h)
        #[arg(short = 'd', long = "older-than")]
        older_than: Option<String>,
        /// Empty the trash
        #[arg(long)]
        all: bool,
        /// Show what would be removed without deleting anything
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum OrphanCommand {
    /// List orphaned metarecords (uuid, stale path) — the default
    List,
    /// Orphan the scanned records: freeze mfr_path_old, set mfr_path = Nothing
    Clear {
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum MountCommand {
    /// List the declared mount points and their state — the default
    List {
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
    /// Un-declare a mount point (force-unsets `mfr_mount` on that metarecord)
    Forget {
        /// The mount point's metarecord uuid
        uuid: String,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum WatchCommand {
    /// Show whether ingestion is running or paused — the default
    Status {
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
    /// Stop the flush in progress and pause ingestion (nothing is lost)
    Pause {
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
    /// Resume ingestion and apply what was buffered meanwhile
    Resume {
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SyncCommand {
    /// Compute a sync plan (into a freshly recreated plan repo)
    Plan {
        repo_a: String,
        repo_b: String,
        /// Path to the TOML intents file (scope + policies)
        #[arg(long)]
        intents: PathBuf,
        /// Repo (of the pair) to host the plan + sync database (default: canonical A)
        #[arg(long)]
        host: Option<String>,
        /// Override every conflict rule: ask | skip | prefer:<repo>
        #[arg(long = "on-conflict")]
        on_conflict: Option<String>,
    },
    /// Execute the current plan (mutating the two repos), then prune succeeded ops
    Run {
        repo_a: String,
        repo_b: String,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Render the current plan with live red/green status (default: summary + reds)
    Show {
        repo_a: String,
        repo_b: String,
        /// Only the conflict operations
        #[arg(long)]
        conflicts: bool,
        /// Only the file operations (copy/move/chmod/delete)
        #[arg(long)]
        files: bool,
        /// Only the per-kind counts
        #[arg(long)]
        summary: bool,
    },
    /// Print the per-link change/conflict states of a repo pair
    Status {
        /// The two repositories (name or UUID; order does not matter)
        repo_a: String,
        repo_b: String,
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
    /// Manually link a record of repo_a to a record of repo_b
    Link {
        repo_a: String,
        repo_b: String,
        /// Record UUID in repo_a
        uuid_a: String,
        /// Record UUID in repo_b
        uuid_b: String,
        /// Repo (of the pair) to host the sync database when created first
        #[arg(long)]
        host: Option<String>,
    },
    /// Remove a link (optionally deleting one endpoint record first)
    Unlink {
        repo_a: String,
        repo_b: String,
        /// The link UUID (from `mf sync status`)
        link: String,
        /// Also delete the endpoint record in `a` (repo_a) or `b` (repo_b)
        #[arg(long = "with-endpoint", value_parser = ["a", "b"])]
        with_endpoint: Option<String>,
    },
}

#[derive(Subcommand)]
enum GuiCommand {
    /// Print the GUI state (pretty-printed JSON)
    Status,
    /// Print the active repository of the focused workspace
    Repo,
    /// Workspace (tab) management
    Workspace {
        #[command(subcommand)]
        command: GuiWorkspaceCommand,
    },
    /// Print or assign the slot layout ('-' = hidden slot)
    Layout {
        /// Slot name (left or right)
        slot: Option<String>,
        /// Workspace id to assign, or '-' to hide the slot
        value: Option<String>,
    },
    /// Print or set the panel type shown in a slot
    View {
        /// Slot name (left or right)
        slot: String,
        /// Panel type to set (omit to print the current one)
        panel_type: Option<String>,
        /// File path (file panel type)
        #[arg(long)]
        path: Option<String>,
        /// Initial panel state as a JSON object
        #[arg(long)]
        state: Option<String>,
    },
    /// Post a message to a workspace's status bar
    Message {
        text: String,
        /// Target workspace id (default: the focused workspace)
        #[arg(long)]
        workspace: Option<String>,
        /// Auto-clear delay; persistent when omitted
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Report progress for the running script (updates its task-bar entry)
    Progress {
        /// Items processed so far (drives a determinate bar with --total)
        #[arg(long)]
        done: Option<u64>,
        /// Total items to process
        #[arg(long)]
        total: Option<u64>,
        /// Short label for the current step (e.g. the file being processed)
        #[arg(long)]
        phase: Option<String>,
        /// Run id (default: the METAFOLDER_GUI_TASK the GUI injected)
        #[arg(long)]
        task: Option<String>,
    },
    /// Print the recorded bench measures (JSON), or clear the buffer
    Bench {
        /// Empty the bench buffer instead of printing it
        #[arg(long)]
        clear: bool,
    },
    /// Run a command invocation through the GUI (same as the command input)
    Command {
        /// The command invocation, e.g. `panel:set-type file`
        #[arg(required = true, trailing_var_arg = true)]
        invocation: Vec<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Wait for one of the given keys and print it
    Input {
        /// Keys to bind for the duration of the wait (e.g. y n escape)
        #[arg(required = true)]
        keys: Vec<String>,
        /// Question shown in the GUI's dedicated input bar while waiting
        /// (kept separate from status/error messages)
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Prompt the user in the command input and print the answer
    Prompt {
        text: String,
        /// Autocomplete value offered during the prompt; repeatable
        #[arg(long = "completion")]
        completions: Vec<String>,
        /// Read more completions from stdin (one per line, empty line ends)
        #[arg(long)]
        completions_stdin: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
}

#[derive(Subcommand)]
enum GuiWorkspaceCommand {
    /// Create a workspace and print its id
    New {
        /// Active repository UUID (default: the daemon's first repo)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Close a workspace
    Rm { id: String },
}

/// A rollback/prune target: a revision label, --id, or --timestamp.
#[derive(clap::Args)]
struct TargetOpts {
    /// Revision label (most recent on the HEAD ancestry path)
    target: Option<String>,
    /// Target operation by id
    #[arg(long)]
    id: Option<i64>,
    /// Most recent operation whose revision timestamp ≤ T (ISO-8601, or @<unix-ms>)
    #[arg(long)]
    timestamp: Option<String>,
}

impl TargetOpts {
    fn into_args(self) -> metafolder_cli::log::TargetArgs {
        metafolder_cli::log::TargetArgs {
            label: self.target,
            id: self.id,
            timestamp: self.timestamp,
        }
    }
    fn is_empty(&self) -> bool {
        self.target.is_none() && self.id.is_none() && self.timestamp.is_none()
    }
}

#[derive(Subcommand)]
enum PruneCommand {
    /// Make <target> the new root, deleting all older operations
    Before {
        #[command(flatten)]
        target: TargetOpts,
        /// Skip the confirmation prompt
        #[arg(long)]
        force: bool,
        /// Suppress informational output
        #[arg(long)]
        silent: bool,
    },
    /// Delete branch operations diverging from the HEAD path up to <target>
    Linearize {
        #[command(flatten)]
        target: TargetOpts,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        silent: bool,
    },
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Check metarecords against the schema and list the violations
    Check {
        /// Restrict the check to metarecords matching this predicate
        predicate: Option<String>,
        /// Print the raw JSON response body
        #[arg(long)]
        json: bool,
    },
    /// Re-read the schema file
    Reload,
    /// Print the loaded schema (pretty-printed JSON)
    Show,
}

fn main() {
    let cli = Cli::parse();
    // Load the config first (unless --no-config); a malformed file aborts before
    // any daemon round-trip. Precedence: an explicit flag/env wins over config.
    let config = match metafolder_cli::config::load(cli.no_config) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(2);
        }
    };
    let port = cli.port.unwrap_or(config.settings.daemon_port);
    // The default [repo] applies as a whole and only when the user named
    // neither -n nor -u (nor their env vars); an explicit selector of either
    // kind fully overrides it (so the config never fills the *other* field and
    // trips the "-n and -u are mutually exclusive" check).
    let (repo_name, repo_uuid) = match (cli.repo_name, cli.repo_uuid) {
        (None, None) => (config.repo.name.clone(), config.repo.uuid.clone()),
        explicit => explicit,
    };
    let ctx = Ctx::new(port, repo_name, repo_uuid, &config);
    let result = dispatch(&ctx, cli.command);
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {}", error.message());
            std::process::exit(error.exit_code());
        }
    }
}

type CmdResult = Result<i32, metafolder_cli::client::CliError>;

fn dispatch(ctx: &Ctx, command: Command) -> CmdResult {
    match command {
        Command::Repo { command } => match command.unwrap_or(RepoCommand::List { all: false }) {
            RepoCommand::List { all } => commands::repos(ctx, all),
            RepoCommand::Init { root, metafolder, ignore, no_ignore } => {
                commands::init(ctx, &root, metafolder.as_deref(), ignore, no_ignore)
            }
            RepoCommand::Load { root, metafolder, no_wait } => {
                commands::load(ctx, root.as_deref(), metafolder.as_deref(), no_wait)
            }
            RepoCommand::Unload => commands::unload(ctx),
        },
        Command::Task { command } => {
            match command.unwrap_or(TaskCommand::List { all: false, json: false }) {
                TaskCommand::List { all, json } => commands::tasks(ctx, all, json),
                TaskCommand::Show { id, stop, json } => commands::task(ctx, &id, stop, json),
            }
        }
        Command::Log { command } => dispatch_log(ctx, command),
        Command::Trash { file, command } => dispatch_trash(ctx, file, command),
        Command::Watch { command } => {
            match command.unwrap_or(WatchCommand::Status { json: false }) {
                WatchCommand::Status { json } => commands::watch_status(ctx, json),
                WatchCommand::Pause { json } => commands::watch_pause(ctx, json),
                WatchCommand::Resume { json } => commands::watch_resume(ctx, json),
            }
        }
        Command::Mount { command } => match command.unwrap_or(MountCommand::List { json: false }) {
            MountCommand::List { json } => commands::mount_list(ctx, json),
            MountCommand::Forget { uuid, yes } => commands::mount_forget(ctx, &uuid, yes),
        },
        Command::Orphan { command } => match command.unwrap_or(OrphanCommand::List) {
            OrphanCommand::List => commands::orphan_list(ctx),
            OrphanCommand::Clear { yes } => commands::orphan_clear(ctx, yes),
        },
        Command::Metarecord { query, id, eq, simplified, verb } => {
            dispatch_metarecord(ctx, query, id, eq, simplified, verb)
        }
        Command::Field { command } => dispatch_field(ctx, command),
        Command::Retype { name, to } => commands::retype(ctx, &name, &to),
        Command::Reconcile {
            metarecord,
            threshold,
            no_mime,
            no_metadata,
            no_refresh,
            json,
            no_wait,
            poll_interval,
        } => commands::reconcile(
            ctx,
            metarecord.as_deref(),
            threshold,
            !no_mime,
            !no_metadata,
            !no_refresh,
            json,
            no_wait,
            poll_interval.unwrap_or(ctx.reconcile_poll_interval_ms),
        ),
        Command::Track { path } => commands::track(ctx, &path),
        Command::Path { uuid, relative } => commands::path(ctx, &uuid, relative),
        Command::Order { path, meta, max_gap, dry_run } => {
            commands::order(ctx, &path, &meta, max_gap, dry_run)
        }
        Command::Tag { query, id, eq, simplified, verb } => {
            dispatch_tag(ctx, query, id, eq, simplified, verb)
        }
        Command::Ignore { command } => dispatch_ignore(ctx, command),
        Command::Schema { command } => match command {
            SchemaCommand::Check { predicate, json } => {
                commands::schema_check(ctx, predicate.as_deref(), json)
            }
            SchemaCommand::Reload => commands::schema_reload(ctx),
            SchemaCommand::Show => commands::schema_show(ctx),
        },
        Command::Gui { gui_url, command } => dispatch_gui(gui_url, command),
        Command::Sync { command } => dispatch_sync(ctx, command),
    }
}

fn dispatch_sync(ctx: &Ctx, command: SyncCommand) -> CmdResult {
    match command {
        SyncCommand::Plan { repo_a, repo_b, intents, host, on_conflict } => sync::plan::run(
            ctx,
            &repo_a,
            &repo_b,
            &intents,
            host.as_deref(),
            on_conflict.as_deref(),
        ),
        SyncCommand::Run { repo_a, repo_b, yes } => sync::run::run(ctx, &repo_a, &repo_b, yes),
        SyncCommand::Show { repo_a, repo_b, conflicts, files, summary } => {
            sync::run::show(ctx, &repo_a, &repo_b, conflicts, files, summary)
        }
        SyncCommand::Status { repo_a, repo_b, json } => sync::status(ctx, &repo_a, &repo_b, json),
        SyncCommand::Link { repo_a, repo_b, uuid_a, uuid_b, host } => {
            sync::link(ctx, &repo_a, &repo_b, &uuid_a, &uuid_b, host.as_deref())
        }
        SyncCommand::Unlink { repo_a, repo_b, link, with_endpoint } => {
            sync::unlink(ctx, &repo_a, &repo_b, &link, with_endpoint.as_deref())
        }
    }
}

fn dispatch_tag(
    ctx: &Ctx,
    query: Option<String>,
    id: Option<String>,
    eq: Vec<String>,
    simplified: bool,
    verb: TagVerb,
) -> CmdResult {
    use metafolder_cli::client::CliError::Usage;
    if let TagVerb::List = verb {
        return commands::tag_list(ctx);
    }
    let selector = commands::resolve_selector(query.as_deref(), id.as_deref(), &eq, simplified)?
        .ok_or_else(|| Usage("mf tag needs a selector: -i, -q or --eq".into()))?;
    match verb {
        TagVerb::Add { path } => commands::tag_add(ctx, &selector, &path),
        TagVerb::Deny { path } => commands::tag_deny(ctx, &selector, &path),
        TagVerb::Mixed { path } => commands::tag_mixed(ctx, &selector, &path),
        TagVerb::Remove { path } => commands::tag_remove(ctx, &selector, &path),
        TagVerb::List => unreachable!("handled above"),
    }
}

fn dispatch_metarecord(
    ctx: &Ctx,
    query: Option<String>,
    id: Option<String>,
    eq: Vec<String>,
    simplified: bool,
    verb: Option<MetarecordVerb>,
) -> CmdResult {
    use metafolder_cli::client::CliError::Usage;
    let by_id = id.is_some();
    // -q is expanded here when -s is set, so the rest sees a normal-DSL selector.
    let selector = commands::resolve_selector(query.as_deref(), id.as_deref(), &eq, simplified)?;
    let verb = verb.unwrap_or(MetarecordVerb::Get {
        select: None,
        sort: Vec::new(),
        limit: None,
        values: false,
        tsv: false,
        resolve_tree: None,
    });
    match verb {
        MetarecordVerb::Get { select, sort, limit, values, tsv, resolve_tree } => {
            commands::metarecord_get(
                ctx,
                selector.as_deref(),
                select.as_deref(),
                &sort,
                limit,
                values,
                tsv,
                resolve_tree.as_deref(),
            )
        }
        MetarecordVerb::Add { specs, force } => {
            if selector.is_some() {
                return Err(Usage(
                    "mf metarecord add creates a new metarecord and takes no selector".into(),
                ));
            }
            commands::create(ctx, &specs, force)
        }
        MetarecordVerb::Set { specs, force } => match selector {
            Some(uuid) if by_id => commands::metarecord_set(ctx, &uuid, &specs, force),
            _ => Err(Usage("mf metarecord set requires -i <uuid> (whole-record overwrite)".into())),
        },
        MetarecordVerb::Delete { force } => {
            let sel =
                selector.ok_or_else(|| Usage("mf metarecord delete requires -q or -i".into()))?;
            commands::delete(ctx, &sel, force)
        }
        MetarecordVerb::Field { verb } => {
            let sel = selector
                .ok_or_else(|| Usage("a field operation requires a selector (-q or -i)".into()))?;
            match verb {
                FieldVerb::Get { name, resolve } => {
                    commands::field_get(ctx, &sel, &name, resolve.as_deref())
                }
                FieldVerb::Set { specs, force } => commands::field_set(ctx, &sel, &specs, force),
                FieldVerb::Add { spec, force } => commands::add(ctx, &sel, &spec, force),
                FieldVerb::Delete { spec, force } => commands::remove(ctx, &sel, &spec, force),
                FieldVerb::Unset { name, force } => commands::field_unset(ctx, &sel, &name, force),
            }
        }
    }
}

fn dispatch_field(ctx: &Ctx, command: Option<FieldCommand>) -> CmdResult {
    match command {
        // `list` is the group's default (mf field ≡ mf field list).
        None => commands::field_list(ctx, None),
        Some(FieldCommand::List { type_filter }) => {
            commands::field_list(ctx, type_filter.as_deref())
        }
        Some(FieldCommand::Get { id }) => commands::field_by_id_get(ctx, id),
        Some(FieldCommand::Set { id, spec, force }) => {
            commands::field_by_id_set(ctx, id, &spec, force)
        }
        Some(FieldCommand::Delete { id, force }) => commands::field_by_id_delete(ctx, id, force),
    }
}

fn dispatch_ignore(ctx: &Ctx, command: Option<IgnoreCommand>) -> CmdResult {
    use metafolder_core::ignore::Mode;
    match command.unwrap_or(IgnoreCommand::List { dir: None }) {
        IgnoreCommand::List { dir } => commands::ignore_list(ctx, dir.as_deref()),
        IgnoreCommand::Add(a) => {
            commands::ignore_apply(ctx, a.dir.as_deref(), &a.presets, Mode::Add)
        }
        IgnoreCommand::Remove(a) => {
            commands::ignore_apply(ctx, a.dir.as_deref(), &a.presets, Mode::Remove)
        }
        IgnoreCommand::Set(a) => {
            commands::ignore_apply(ctx, a.dir.as_deref(), &a.presets, Mode::Set)
        }
    }
}

fn dispatch_log(ctx: &Ctx, command: Option<LogCommand>) -> CmdResult {
    match command {
        None => log::log(ctx, &log::LogArgs::default()),
        Some(LogCommand::List { tree, graph, ops, metarecord, limit, since, until, all }) => {
            log::log(ctx, &log::LogArgs { tree, graph, ops, metarecord, limit, since, until, all })
        }
        Some(LogCommand::Show { target, raw }) => log::log_show(ctx, &target, raw),
        Some(LogCommand::Rollback {
            args,
            id,
            timestamp,
            on_move_available,
            on_move_unavailable,
            silent,
        }) => {
            let (is_plan, label) = match args.split_first() {
                Some((first, rest)) if first == "plan" => (true, rest.first().cloned()),
                Some((first, _)) => (false, Some(first.clone())),
                None => (false, None),
            };
            let target = log::TargetArgs { label, id, timestamp };
            if is_plan {
                log::rollback_plan(ctx, target)
            } else {
                let policies = (|| {
                    Ok::<_, metafolder_cli::client::CliError>(log::RollbackPolicies {
                        on_available: on_move_available
                            .as_deref()
                            .map(log::Policy::parse)
                            .transpose()?
                            .unwrap_or(log::Policy::Apply),
                        on_unavailable: on_move_unavailable
                            .as_deref()
                            .map(log::Policy::parse)
                            .transpose()?
                            .unwrap_or(log::Policy::Ask),
                    })
                })();
                match policies {
                    Ok(policies) => log::rollback_run(ctx, target, policies, silent),
                    Err(e) => Err(e),
                }
            }
        }
        Some(LogCommand::Prune { command }) => match command {
            PruneCommand::Before { target, force, silent } => {
                if target.is_empty() {
                    Err(metafolder_cli::client::CliError::Usage(
                        "mf log prune before requires a target (<label>, --id, or --timestamp)"
                            .into(),
                    ))
                } else {
                    log::prune(ctx, "before", target.into_args(), force, silent)
                }
            }
            PruneCommand::Linearize { target, force, silent } => {
                if target.is_empty() {
                    Err(metafolder_cli::client::CliError::Usage(
                        "mf log prune linearize requires a target (<label>, --id, or --timestamp)"
                            .into(),
                    ))
                } else {
                    log::prune(ctx, "linearize", target.into_args(), force, silent)
                }
            }
        },
    }
}

fn dispatch_trash(ctx: &Ctx, file: Option<PathBuf>, command: Option<TrashCommand>) -> CmdResult {
    if let Some(path) = file {
        if command.is_some() {
            return Err(metafolder_cli::client::CliError::Usage(
                "mf trash -f <file> cannot be combined with a subcommand".into(),
            ));
        }
        return commands::trash_add(ctx, &path);
    }
    match command.unwrap_or(TrashCommand::List) {
        TrashCommand::List => commands::trash_list(ctx),
        TrashCommand::Restore { id } => commands::trash_restore(ctx, &id),
        TrashCommand::Prune { size, older_than, all, dry_run } => {
            let mode = commands::trash_prune_mode(size.as_deref(), older_than.as_deref(), all)?;
            commands::trash_prune(ctx, mode, dry_run)
        }
    }
}

fn dispatch_gui(gui_url: Option<String>, command: GuiCommand) -> CmdResult {
    let url = gui::base_url(gui_url, &gui::config_path_candidates());
    let gui_ctx = GuiCtx::new(&url);
    match command {
        GuiCommand::Status => gui::status(&gui_ctx),
        GuiCommand::Repo => gui::repo(&gui_ctx),
        GuiCommand::Workspace { command } => match command {
            GuiWorkspaceCommand::New { repo } => gui::workspace_new(&gui_ctx, repo.as_deref()),
            GuiWorkspaceCommand::Rm { id } => gui::workspace_rm(&gui_ctx, &id),
        },
        GuiCommand::Layout { slot, value } => {
            gui::layout(&gui_ctx, slot.as_deref(), value.as_deref())
        }
        GuiCommand::View { slot, panel_type, path, state } => {
            gui::view(&gui_ctx, &slot, panel_type.as_deref(), path.as_deref(), state.as_deref())
        }
        GuiCommand::Progress { done, total, phase, task } => {
            gui::progress(&gui_ctx, done, total, phase.as_deref(), task)
        }
        GuiCommand::Message { text, workspace, timeout_ms } => {
            gui::message(&gui_ctx, &text, workspace.as_deref(), timeout_ms)
        }
        GuiCommand::Bench { clear } => gui::bench(&gui_ctx, clear),
        GuiCommand::Command { invocation, timeout_ms } => {
            gui::command(&gui_ctx, &invocation.join(" "), timeout_ms)
        }
        GuiCommand::Input { keys, prompt, timeout_ms } => {
            gui::input(&gui_ctx, &keys, prompt.as_deref(), timeout_ms)
        }
        GuiCommand::Prompt { text, completions, completions_stdin, timeout_ms } => {
            gui::prompt(&gui_ctx, &text, &completions, completions_stdin, timeout_ms)
        }
    }
}
