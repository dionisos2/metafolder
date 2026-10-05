// The `metafolder` object every panel receives as `mount(root, metafolder)`
// (doc "The metafolder API"), and the data-model shapes it carries.
//
// This file is a *script*, not a module: it has no top-level import or export,
// so its declarations are global to the whole TypeScript program. That is what
// lets the panel JS — which lives outside frontend/, at
// crates/gui/default-config/panel-types/ — write
//
//     /** @param {MetafolderApi} metafolder */
//
// with no import path to get wrong. (Types pulled in from real modules use
// inline `import('...')` types, which keep the file a script; a top-level
// `import` statement would make it a module and silently destroy the globality
// every panel depends on.)
//
// The implementation is src/lib/panels/api.ts, which is annotated
// `const api: MetafolderApi = {...}` — so the two cannot drift.

declare namespace Metafolder {
  // ── The data model (doc "Data model") ────────────────────────────────────

  /**
   * A field value. `nothing` is explicit absence and carries no `value`.
   *
   * The tags are what serde emits for `core::metarecord::Value`, which is
   * `rename_all = "lowercase"` — hence `refbase` and `externalref` with no
   * underscore. Only `TreeRef` carries an explicit rename, to `tree_ref`.
   */
  type Value =
    | { type: 'nothing' }
    | { type: 'string' | 'datetime'; value: string }
    | { type: 'int' | 'float'; value: number }
    | { type: 'bool'; value: boolean }
    | { type: 'ref' | 'refbase'; value: string }
    | { type: 'tree_ref'; value: TreeRef }
    | { type: 'externalref'; value: { repo: string; metarecord: string } };

  /** A `tree_ref` value: the parent metarecord (null at a forest root) + name. */
  interface TreeRef {
    parent: string | null;
    name: string;
  }

  /** One field row. Fields are a multi-map: several may share a name. */
  interface Field {
    /** The DB row id, present in API responses. */
    id?: number;
    name: string;
    value: Value;
  }

  interface Metarecord {
    uuid: string;
    version?: number;
    fields?: Field[];
  }

  /** What a daemon read may be given: a `signal` to abort it while in flight. */
  interface ReadOptions {
    signal?: AbortSignal;
  }

  /** A daemon proxy response, as `daemon.request` returns it. */
  type DaemonResponse = import('../src/lib/panels/reads.js').DaemonResponse;

  // ── The API surface ───────────────────────────────────────────────────────

  /** Shared panel UX timing knobs (config.toml `[panels]`). Every key may be
   *  absent when the config is minimal, so read them as `?? <fallback>`. */
  interface Settings {
    statusMessageMs?: number;
    statusErrorMs?: number;
    finderDebounceMs?: number;
    livePreviewDebounceMs?: number;
    taskPollMs?: number;
  }

  /** Per-panel-type defaults from `config.toml` (`[panel-defaults.<type>]`),
   *  camelCased. Every key is optional: an unconfigured one is undefined and
   *  the panel falls back to its own module constant. The built-in panels'
   *  keys are declared for type-checking; the index signature carries a custom
   *  panel type's own keys, which the GUI knows nothing about. */
  interface Defaults {
    /** metarecord-list: initial table columns (columns-input syntax). */
    columns?: string;
    /** metarecord-list: fields the finder searches (`field[:aspect]`). */
    finderFields?: string[];
    /** metarecord-list: the column shown as a tile's name in grid mode. */
    gridNameColumn?: string;
    /** treeref: the tree_ref field whose forest the panel opens on. */
    field?: string;
    /** treeref: the Ref field `treeref:list-refs` follows back into the forest. */
    refField?: string;
    /** file: extensions taking the text-preview fast path. */
    textExtensions?: string[];
    /** file: bytes of a text file read into the preview. */
    textPreviewLimit?: number;
    /** file: image zoom step (a multiplier) and bounds. */
    zoomStep?: number;
    zoomMin?: number;
    zoomMax?: number;
    readonly [key: string]: unknown;
  }

  interface Bench {
    measure<T>(name: string, fn: () => T): T;
    record(name: string, durationMs: number): void;
  }

  interface Daemon {
    /** Declares what the user asked this panel for, in their own words (a
     *  query's DSL text). Kept with the panel's daemon calls and written to the
     *  slow-operation log when one is slow — the daemon receives the query IR
     *  and cannot reconstruct the text (doc "Slow log"). Local and free: it sets
     *  a string. Capped at 200 characters; `null` clears it. */
    setContext(text: string | null): void;
    /** The raw round-trip: never throws on a 4xx/5xx, returns `{status, body}`.
     *  With `signal`, aborting drops the call in flight — the daemon cancels
     *  the query it was running — and the promise rejects with an
     *  `AbortError`, as `fetch` does. So do `call` and `query`. */
    request(method: string, path: string, body?: unknown, opts?: ReadOptions): Promise<DaemonResponse>;
    /** As `request`, but throws the daemon's `{"error": …}` message on >= 400. */
    call(method: string, path: string, body?: unknown, opts?: ReadOptions): Promise<unknown>;
    /** The repo-root-relative path of a metarecord's `mfr_path`. */
    resolvePath(repo: string, uuid: string): Promise<string>;
    /** The path a `tree_ref` value names, in the forest of `field` — the field
     *  the value was read from (default `mfr_path`). */
    resolveTreeRef(repo: string, value: TreeRef, field?: string): Promise<string>;
    repoRoot(repo: string): Promise<string>;
    repoInternalDir(repo: string): Promise<string>;
    /** Absolute paths — one, since `mfr_path` is single-valued; a list for the
     *  shape of the endpoint. */
    metarecordPaths(repo: string, metarecord: { uuid: string }): Promise<string[]>;
    /** Creates a repository *and* applies its ignore preset (doc "Ignore presets"),
     * the same orchestration `mf repo init` uses; returns the
     *  new repo's uuid. `call('POST', '/repos/init', …)` would skip the ignore
     *  step, so repository creation must go through here. */
    initRepo(opts: {
      root: string;
      name?: string;
      metafolder?: string;
      noIgnore?: boolean;
      ignore?: string[];
    }): Promise<string>;
    // Reads that return what they read. Nothing is kept anywhere: each is a
    // daemon round-trip, so the panel keeps what it displays and re-reads when
    // `changes` says it moved.
    /** One page of a query: uuids + records + cursor + optional total. */
    query(
      repo: string,
      body: Record<string, unknown>,
      opts?: ReadOptions,
    ): Promise<{
      uuids: string[];
      records: Metarecord[];
      nextCursor: string | null;
      total: number | null;
    }>;
    /** The named metarecords that exist, by uuid. */
    metarecords(repo: string, uuids: string[]): Promise<Map<string, Metarecord>>;
    /** Each named metarecord's resolved positions in a TreeRef field (`[]`
     *  for one without any). */
    treePaths(repo: string, field: string, uuids: string[]): Promise<Record<string, string[]>>;
    /** The repository's field catalogue: each distinct name and its type. */
    fields(repo: string): Promise<{ name: string; type: string }[]>;
  }

  /** The daemon change feed (GET /log/since): what changed that the panel did
   *  not write itself. */
  interface Changes {
    /** Poll the feed now — a deliberate freshness point (a refresh, a
     *  catch-up after a disk change), on top of the background timer. */
    sync(repo: string): Promise<void>;
    /** Runs `cb` on each change (`uuids` are the touched metarecords, `null`
     *  the whole repository). Returns an unsubscribe fn — call it in the
     *  panel's cleanup. */
    subscribe(cb: (event: Change) => void): () => void;
  }

  /** The payload of a {@link Changes.subscribe} notification. */
  interface Change {
    repo: string;
    uuids: string[] | null;
  }

  /** Pure query transformations, run locally in the GUI backend (core). */
  interface Query {
    parse(dsl: string): Promise<unknown>;
    expand(simplified: string): Promise<unknown>;
    /** The simplified-query grammar source as loaded at startup (help page). */
    grammarSource(): Promise<string>;
  }

  /** Value picker (doc "Value picker"): opens a linked picker workspace
   *  whose confirmed selection comes back as the `pick_result` workspace
   *  variable, matched by `token`. */
  interface Pick {
    start(spec: Record<string, unknown>): Promise<string>;
  }

  interface PanelConfig {
    /** The `[ref-seeds]` rule naming a `ref` field's targets —
     *  `{query, columns}` (doc "Ref value seeds"): which metarecords may
     *  be named, and how each is shown (the `metarecord-list` query and columns
     *  syntaxes). The named rule wins, `"*"` is the default; null when neither
     *  exists. */
    refSeed(field: string): Promise<{ query: string | null; columns: string } | null>;
    /** What joins the columns of a multi-column completion label (config.toml
     *  `[completion].label-separator`, doc "Completion views"). */
    labelSeparator(): Promise<string>;
  }

  interface Workspace {
    get(key: string): Promise<unknown>;
    set(key: string, value: unknown): Promise<void>;
    /** Every variable the workspace holds now, `active_repo` included — what a
     *  panel built after some were set starts from (`onChange` only reports
     *  what moves afterwards). */
    all(): Promise<Record<string, unknown>>;
    adoptRepo(repo: string): Promise<void>;
    /** Subscribe to one variable, or to `'*'` for every change (the listener
     *  then also receives the key). */
    onChange(key: string, listener: (value: unknown, key?: string) => void): void;
  }

  /** One completion candidate (doc "Completion views"): the label shown
   *  (and matched whole) and the value the command receives. A plain string is
   *  both. */
  type CompletionItem = string | { label: string; value: string };

  /** One page of candidates; `more` says the source holds more than it handed
   *  over, so narrowing on the typed text means asking again. */
  type CompletionPage = { items: CompletionItem[]; more?: boolean };

  /** One cycled view of a completion: its title while on screen, and its
   *  candidates for the typed text and the arguments collected so far. */
  interface CompletionView {
    title?: string;
    items(
      partial: string,
      prior: string[],
    ):
      | CompletionItem[]
      | CompletionPage
      | Promise<CompletionItem[] | CompletionPage>;
  }

  /** One declared command argument (doc "Interactive command arguments"). Its `prompt`,
   *  `initial` and `complete` are functions evaluated *lazily* when the
   *  argument is asked for (never at registration), each receiving the
   *  arguments already collected — so `initial` can read live state (e.g. the
   *  current value of a field). When a command is invoked with fewer
   *  parameters than declared, the command input collects the missing tail. */
  interface CommandArg {
    name: string;
    /** The prompt text shown in the command input. */
    prompt(prior: string[]): string | Promise<string>;
    /** A pre-filled, editable value (empty if omitted). */
    initial?(prior: string[]): string | Promise<string>;
    /** Autocomplete candidates (filtered client-side like command names).
     *  `partial` is the current draft, so a source that talks to the daemon
     *  can narrow as the user types. */
    complete?(partial: string, prior: string[]):
      | CompletionItem[]
      | CompletionPage
      | Promise<CompletionItem[] | CompletionPage>;
    /** The cycled views of the candidates (doc "Completion views"), for
     *  the case where *which* views there are depends on the arguments
     *  collected so far. Wins over `complete`. */
    views?(prior: string[]): CompletionView[] | Promise<CompletionView[]>;
    /** Whether the argument may be left out. Supplied inline it is used;
     *  omitted it is skipped rather than prompted, so the command falls back
     *  to its default (`log:revert` vs `log:revert with-dependents`). */
    optional?: boolean;
    /** Whether the argument applies at all, given the ones already collected.
     *  A generic command declares the union of its operations' arguments and
     *  drops the ones an operation has no use for. */
    when?(prior: string[]): boolean;
  }

  interface CommandOptions {
    label?: string;
    /** Put the owning panel on screen before running, when it is not
     *  (doc "Writing a panel type"). */
    reveal?: boolean;
    /** Append the invocation to the workspace message log (default true). */
    log?: boolean;
    handler?: (...args: string[]) => unknown;
    /** Declared arguments, collected interactively when missing. */
    args?: CommandArg[];
  }

  interface Commands {
    register(name: string, options?: CommandOptions): Promise<unknown>;
    invoke(invocation: string): unknown;
    /** The compiled keybinding table as it stands now — what the user's
     *  `keybindings.toml` and the panel suggestions add up to. A snapshot: call
     *  it again to see a rebinding (doc "How the help panel finds a page"). */
    keybindings(): Promise<Binding[]>;
  }

  /** One compiled keybinding (`CompiledBinding`): a combo *sequence*, the
   *  invocation it runs, and its scope. */
  interface Binding {
    keys: string[];
    invocation: string;
    when: string | null;
    text_input: boolean;
    focus: string | null;
  }

  interface FsEntry {
    /** The name as it is shown and typed: itself when the name is text, its
     *  `%XX` escaped form when it holds bytes no text can represent. */
    name: string;
    /** The same escaping applied to the whole path. It is a *handle*: hand it
     *  back unchanged (fs commands, `?path=` URLs) and the backend turns it
     *  into the exact bytes. Slicing and joining it works as for a plain
     *  path. */
    path: string;
    is_dir: boolean;
    /** The name needed escaping, so what is shown is not literally what the
     *  disk holds — the row is marked and the file is worth renaming. */
    escaped?: boolean;
  }

  interface Fs {
    readDir(path: string): Promise<FsEntry[]>;
    /** `{path, is_dir, size, mtime, kind}`, following links. `kind` is what
     *  the file *is*, read from its first bytes: `image`, `gif`, `video`,
     *  `audio`, `document`, or null (a directory, text, anything else). */
    stat(path: string): Promise<unknown>;
    /** Is anything at `path` — the entry itself, a broken symlink included?
     *  `stat` follows links (so a link to a directory reads as a directory);
     *  this is the one to ask "is it still there?". */
    exists(path: string): Promise<boolean>;
    homeDir(): Promise<string>;
    /** Creates a single new directory (its parent must exist); errors if it
     *  already exists. */
    mkdir(path: string): Promise<void>;
    /** Creates a new empty file; errors if it already exists (never truncates). */
    createFile(path: string): Promise<void>;
    /** Moves/renames a path (cross-filesystem safe); refuses to overwrite `to`. */
    move(from: string, to: string): Promise<void>;
    /** Copies a file or directory tree, leaving the source; refuses to overwrite. */
    copy(from: string, to: string): Promise<void>;
    /** Permanently deletes a file, symlink, or directory tree. */
    remove(path: string): Promise<void>;
  }

  /** One entry in a repository's trash-bin (doc "Trash"). */
  interface TrashEntry {
    id: string;
    original_path: string;
    original_name: string;
    trashed_at: number;
    size: number;
    is_dir: boolean;
    reason: 'rollback' | 'sync' | 'manual';
    revision?: number | null;
    metarecord?: string | null;
    version?: number | null;
  }

  /** Repository trash-bin (doc "Trash"): filesystem operations shared with
   *  the CLI, driven through the trash Tauri commands. No daemon endpoint. */
  interface Trash {
    list(repo: string): Promise<TrashEntry[]>;
    /** Restores an entry to its original path; returns that path. */
    restore(repo: string, id: string): Promise<string>;
    /** Permanently deletes a single entry. */
    remove(repo: string, id: string): Promise<void>;
    /** Empties the whole trash; returns the number of entries removed. */
    empty(repo: string): Promise<number>;
    /** Sends a raw filesystem path to the trash and returns the trashed
     *  basename. A *tracked* path loses its metarecords on the way (captured
     *  and deleted before the bytes move, doc "Trash") — the file-manager
     *  panel's delete, and `metarecord:remove`'s OK. */
    trashPath(repo: string, path: string): Promise<string>;
    /** Sends the files of every metarecord `query` matches to the trash, their
     *  metarecords deleted in one revision (doc "Sending files to the trash").
     *  Posts the outcome to the status bar itself. `metarecord:bulk <target>
     *  trash`. */
    trashQuery(repo: string, query: unknown): Promise<BulkTrashOutcome>;
  }

  /** What `trash.trashQuery` did. */
  interface BulkTrashOutcome {
    /** Trash entries made, one per file or directory moved. */
    trashed: number;
    /** Metarecords under a trashed directory of the set (they went with it). */
    inside: number;
    /** Metarecords with no file, left alone. */
    without_file: number;
    /** Whether the set held the repository root, which is never trashed. */
    root_kept: boolean;
    /** `path: reason` for each file whose bytes could not be moved. */
    failed: string[];
  }

  /** What `log.rollback` did. */
  interface Navigated {
    total: number;
    processed: number;
    /** What the file actions had to say (a file moved, one brought back…). */
    notes: string[];
  }

  /** What `log.revert` wrote: the daemon's revert answer, with the notes. */
  interface Reverted {
    revision: number | null;
    reverted_operations?: number[];
    skipped_operations?: { op_id: number; reason: string }[];
    notes: string[];
  }

  /** Coordinated navigation of the event log (doc "Filesystem coordination"), files included: a
   * navigation that moves a file, brings one
   *  back from the trash-bin or sends one back there does it, with no question
   *  asked while the repository is locked. Shared with `mf log` through
   *  `core::navigation`. Confirm *before* calling. */
  interface Log {
    /** Moves HEAD to `target` (`{id}`, `{timestamp}`, `{label}` or
     *  `{prev_revision: true}`). */
    rollback(repo: string, target: Record<string, unknown>): Promise<Navigated>;
    /** Writes the inverse of `target` (`{rev_id}` or `{op_ids}`) at HEAD;
     *  refused when the plan is blocked and `withDependents` is false. */
    revert(
      repo: string,
      target: Record<string, unknown>,
      withDependents: boolean,
    ): Promise<Reverted>;
  }

  /** Orphaned metarecords (doc "Orphans in the GUI"): the shared `mf orphan` layer,
   *  driven through the orphan Tauri commands. `detect` writes the
   *  `orphan = true` marker and reports its counts itself; `count` reads the
   *  marked set as it stands — detection is not re-run; `delete` removes the
   *  marked metarecords (their files are already gone), reporting the summary
   *  and refreshing the panels itself. */
  interface Orphans {
    /** Marks the orphaned metarecords and unmarks the rest; resolves to how
     *  many carry the marker afterwards. */
    detect(): Promise<number>;
    /** How many metarecords carry the marker right now — what a deletion
     *  prompt names. */
    count(): Promise<number>;
    /** Deletes every marked metarecord; resolves to the deleted count. */
    delete(): Promise<number>;
  }

  /** Per-repo input history (doc "Input history"): GUI-side files under
   *  `.metafolder/gui/history/<zone>`. The store behind `attachHistory`. */
  interface History {
    read(repo: string, zone: string): Promise<string[]>;
    append(repo: string, zone: string, entry: string): Promise<void>;
  }

  /** One entry of the recently-viewed list: a metarecord uuid and the ISO-8601
   *  timestamp of its most recent view. */
  interface RecentEntry {
    uuid: string;
    viewed_at: string;
  }

  /** Per-repo "recently viewed metarecords" (crate::recent): a GUI-side LRU
   *  list under `.metafolder/gui/recent`, newest first. `touch` records a view;
   *  `list` reads back the newest `limit` entries (all when omitted). */
  interface Recent {
    list(repo: string, limit?: number): Promise<RecentEntry[]>;
    touch(repo: string, uuid: string): Promise<void>;
  }

  /** Cross-repo synchronisation (doc "Sync"): the shared `core::sync`
   *  orchestration, driven through the sync Tauri commands. Repos are named
   *  positionally (name or UUID), order-independent. `plan`/`run` run
   *  non-interactively — conflicts are left unresolved for `plan_resolve`
   *  editing in the plan repo. */
  /** Ignore presets (doc "Setting ignore patterns"): the GUI half of `mf ignore`.
   *  Expansion reads a config file, so it goes through the backend; the
   *  eligibility / effective-set introspection are plain `daemon.call`s. */
  interface Ignore {
    /** The installed presets, each fully expanded (groups included). */
    presets(): Promise<{ name: string; description: string; patterns: string[] }[]>;
    /** The target metarecord's own `mf_ignore` rows, in order. */
    current(repo: string, target: string): Promise<string[]>;
    /** Applies named presets to the target; returns the resulting rows. */
    apply(
      repo: string,
      target: string,
      presets: string[],
      mode: 'add' | 'remove' | 'set',
    ): Promise<string[]>;
    /** Writes an explicit pattern list (empty unsets the field). */
    write(repo: string, target: string, patterns: string[]): Promise<void>;
  }

  interface Sync {
    /** The raw `/status` body: `{ links: [{ uuid, state }, …] }`. */
    status(repoA: string, repoB: string): Promise<Record<string, unknown>>;
    /** Links two records; returns the new link UUID. */
    link(
      repoA: string,
      repoB: string,
      uuidA: string,
      uuidB: string,
      host?: string,
    ): Promise<{ uuid: string }>;
    /** Removes a link (optionally deleting endpoint `a`/`b` first). */
    unlink(
      repoA: string,
      repoB: string,
      link: string,
      withEndpoint?: string,
    ): Promise<{ uuid: string }>;
    /** Recomputes the plan from an intents file; returns the plan repo UUID,
     *  the op count and any warnings. */
    plan(
      repoA: string,
      repoB: string,
      intentsPath: string,
      host?: string,
      onConflict?: string,
    ): Promise<{ plan_uuid: string; operations: number; warnings: string[] }>;
    /** Executes the plan (always confirmed): `{ status, done, skipped,
     *  divergences, warnings }`. */
    run(repoA: string, repoB: string): Promise<Record<string, unknown>>;
    /** The live red/green overlay of the current plan. */
    show(
      repoA: string,
      repoB: string,
      conflicts: boolean,
      files: boolean,
    ): Promise<Record<string, unknown>>;
  }

  interface StatusBar {
    message(text: string, timeoutMs?: number | null): Promise<void>;
    /** Accepts an Error or anything stringifiable. */
    error(error: unknown, timeoutMs?: number): Promise<void>;
  }

  interface Messages {
    list(): Promise<unknown[]>;
    /** Appends a line to this workspace's persistent message log. */
    append(text: string): Promise<void>;
    onAppend(listener: (entry: unknown) => void): void;
  }

  /** One line of the shell log (doc "shell panel"). */
  interface ShellEntry {
    ts_ms: number;
    /** The run it belongs to: every line of one shell line shares it. */
    run: string;
    kind: 'command' | 'stdout' | 'stderr' | 'status';
    text: string;
  }

  /** What the shell lines run in this workspace printed (doc "shell panel"). */
  interface Shell {
    list(): Promise<ShellEntry[]>;
    /** `entry` is null when the log was cleared. */
    onAppend(listener: (entry: ShellEntry | null) => void): void;
  }

  /** One context-menu entry, a `{header}` category label, or the string `'-'`
   *  for a separator. A header is non-interactive: it groups the entries below
   *  it (metarecord / file / text operations). The menu is normalized before it
   *  is shown — categories come out in the canonical order (Metarecord, File,
   *  Directory, Ignore, Text, View), same-named ones merge, and one separator is
   *  drawn at each category boundary, so a `'-'` of one's own is only needed in
   *  a menu that has no category at all. */
  type MenuEntry = { label: string; action?: () => void; disabled?: boolean };
  type MenuHeader = { header: string };
  type MenuItem = MenuEntry | MenuHeader | '-';

  /** Callable *and* carrying `addDefaultItems` — hence the `Object.assign` in
   *  api.ts: a plain object literal cannot satisfy a call signature. */
  interface ContextMenu {
    (event: MouseEvent, items: MenuItem[]): void;
    /** Items appended to every context menu, shell and panel alike. */
    addDefaultItems(provider: (event: MouseEvent) => MenuItem[]): void;
  }

  /** The object handed to `mount(root, metafolder)`. */
  interface Api {
    /** `mount` runs after init, so there is nothing to wait for; kept for
     *  compatibility with panels that await it. */
    readonly ready: Promise<void>;
    readonly workspaceId: string;
    readonly panelType: string;
    readonly guiServer: string;
    /** For the GUI server's protected routes (`/fsraw`, `/thumbnail`,
     *  `/__media-probe`): append as `?token=` (doc "Session tokens"). */
    readonly sessionToken: string;
    /** The configured progressive-loading page size for this panel type
     *  (config.toml `[page-size]`); undefined for panels without an entry. */
    readonly pageSize: number | undefined;
    readonly settings: Settings;
    /** This panel type's configured defaults (config.toml
     *  `[panel-defaults.<panel-type>]`); `{}` when the user configured none. */
    readonly defaults: Defaults;
    readonly visible: boolean;
    onVisibility(listener: (visible: boolean, slot: string | null) => void): void;
    whenVisible(fn: () => void): void;
    readonly bench: Bench;
    readonly daemon: Daemon;
    readonly changes: Changes;
    readonly query: Query;
    readonly pick: Pick;
    readonly config: PanelConfig;
    readonly workspace: Workspace;
    readonly commands: Commands;
    /** `commands.invoke` lifted to the top of the object
     *  (`withTopLevelInvoke`, doc "User commands"): composing existing
     *  commands is what a command is *for*, so the call it makes most should
     *  not need a path through the object. Panels get the alias too — one API
     *  to learn, not two. */
    invoke(invocation: string): unknown;
    /** Suggests a binding for one of this panel's commands. `when` defaults to
     *  this panel type; pass it explicitly to widen or narrow the scope. */
    addKeybinding(
      invocation: string,
      combo: string,
      options?: { when?: string; textInput?: boolean; focus?: string },
    ): Promise<unknown>;
    readonly fs: Fs;
    readonly trash: Trash;
    readonly log: Log;
    readonly orphans: Orphans;
    readonly sync: Sync;
    readonly ignore: Ignore;
    readonly history: History;
    readonly recent: Recent;
    readonly statusBar: StatusBar;
    readonly messages: Messages;
    readonly shell: Shell;
    readonly contextMenu: ContextMenu;
  }
}

/** Ergonomic alias: panels write `@param {MetafolderApi} metafolder`. */
type MetafolderApi = Metafolder.Api;
