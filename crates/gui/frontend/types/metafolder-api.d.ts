// The `metafolder` object every panel receives as `mount(root, metafolder)`
// (spec-gui "The metafolder API"), and the data-model shapes it carries.
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
  // ── The data model (spec-data-model) ──────────────────────────────────────

  /** A field value. `nothing` is explicit absence and carries no `value`. */
  type Value =
    | { type: 'nothing' }
    | { type: 'string' | 'datetime'; value: string }
    | { type: 'int' | 'float'; value: number }
    | { type: 'bool'; value: boolean }
    | { type: 'ref' | 'ref_base'; value: string }
    | { type: 'tree_ref'; value: TreeRef }
    | { type: 'external_ref'; value: { repo: string; metarecord: string } };

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

  /** A daemon proxy response, as `daemon.request` returns it. */
  type DaemonResponse = import('../src/lib/panels/cache.js').DaemonResponse;

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

  interface Bench {
    measure<T>(name: string, fn: () => T): T;
    record(name: string, durationMs: number): void;
  }

  interface Daemon {
    /** The raw round-trip: never throws on a 4xx/5xx, returns `{status, body}`. */
    request(method: string, path: string, body?: unknown): Promise<DaemonResponse>;
    /** As `request`, but throws the daemon's `{"error": …}` message on >= 400,
     *  and is transparently served from the shared cache when it can be. */
    call(method: string, path: string, body?: unknown): Promise<unknown>;
    parseQuery(dsl: string): Promise<unknown>;
    expandQuery(simplified: string): Promise<unknown>;
    /** The repo-root-relative path of a metarecord's `mfr_path` (memoized). */
    resolvePath(repo: string, uuid: string): Promise<string>;
    resolveTreeRef(repo: string, value: TreeRef): Promise<string>;
    invalidatePath(repo: string, uuid: string): boolean;
    repoRoot(repo: string): Promise<string>;
    repoInternalDir(repo: string): Promise<string>;
    /** Absolute paths (multi-map: a file may sit at several tree positions). */
    metarecordPaths(repo: string, metarecord: { uuid: string }): Promise<string[]>;
  }

  /** Returned by a synchronous cache read when the datum is absent: render a
   *  placeholder and schedule a refresh. */
  type Refresh = typeof import('../src/lib/panels/cache.js').REFRESH;

  /** The shared daemon-data cache: `fetch*` (async, populates) then `read*`
   *  (sync, for render). Cached data is read-only — never mutate it. */
  interface Cache {
    /** The page itself, not a DaemonResponse: uuids + cursor + optional total. */
    query(
      repo: string,
      body: Record<string, unknown>,
    ): Promise<{ uuids: string[]; nextCursor: string | null; total: number | null }>;
    /** Populates the cache; the data is then read back synchronously. */
    fetchMetarecords(repo: string, uuids: string[]): Promise<void>;
    fetchTreeRefs(repo: string, field: string, uuids: string[]): Promise<void>;
    fetchFields(repo: string): Promise<void>;
    readMetarecord(repo: string, uuid: string): Metarecord | Refresh;
    readTreeRef(repo: string, field: string, uuid: string): string[] | Refresh;
    readFields(repo: string): { name: string; type: string }[] | Refresh;
    /** The single valid type of an existing field, so a form can lock its picker. */
    fieldType(repo: string, name: string): string | null | Refresh;
    /** Poll the change feed now — a deliberate freshness point (a query, a
     *  refresh, a panel becoming visible), on top of the background timer. */
    sync(repo: string): Promise<void>;
    readonly REFRESH: Refresh;
  }

  /** Pure query transformations, run locally in the GUI backend (core). */
  interface Query {
    parse(dsl: string): Promise<unknown>;
    expand(simplified: string): Promise<unknown>;
    /** The simplified-query grammar source as loaded at startup (help page). */
    grammarSource(): Promise<string>;
  }

  /** Value picker (spec-gui "Value picker"): opens a linked picker workspace
   *  whose confirmed selection comes back as the `pick_result` workspace
   *  variable, matched by `token`. */
  interface Pick {
    start(spec: Record<string, unknown>): Promise<string>;
  }

  interface PanelConfig {
    /** The configured `ref` picker seed query for a field, or null
     *  (config.toml `[picker-seeds]`). */
    pickerSeed(field: string): Promise<string | null>;
  }

  interface Workspace {
    get(key: string): Promise<unknown>;
    set(key: string, value: unknown): Promise<void>;
    adoptRepo(repo: string): Promise<void>;
    /** Subscribe to one variable, or to `'*'` for every change (the listener
     *  then also receives the key). */
    onChange(key: string, listener: (value: unknown, key?: string) => void): void;
  }

  interface CommandOptions {
    label?: string;
    textInput?: boolean;
    /** Pre-fill the command input instead of running straight away. */
    reveal?: boolean;
    /** Append the invocation to the workspace message log (default true). */
    log?: boolean;
    handler?: (...args: string[]) => unknown;
  }

  interface Commands {
    register(name: string, options?: CommandOptions): Promise<unknown>;
    invoke(invocation: string): unknown;
  }

  interface FsEntry {
    name: string;
    path: string;
    is_dir: boolean;
  }

  interface Fs {
    readDir(path: string): Promise<FsEntry[]>;
    stat(path: string): Promise<unknown>;
    homeDir(): Promise<string>;
  }

  /** Per-repo input history (spec-gui "Input history"): GUI-side files under
   *  `.metafolder/gui/history/<zone>`. The store behind `attachHistory`. */
  interface History {
    read(repo: string, zone: string): Promise<string[]>;
    append(repo: string, zone: string, entry: string): Promise<void>;
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

  /** One context-menu entry; a separator is `null`. */
  type MenuItem = { label: string; action: () => void } | null;

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
     *  `/__media-probe`): append as `?token=` (spec-auth). */
    readonly sessionToken: string;
    /** The configured progressive-loading page size for this panel type
     *  (config.toml `[page-size]`); undefined for panels without an entry. */
    readonly pageSize: number | undefined;
    readonly settings: Settings;
    readonly visible: boolean;
    onVisibility(listener: (visible: boolean, slot: string | null) => void): void;
    whenVisible(fn: () => void): void;
    readonly bench: Bench;
    readonly daemon: Daemon;
    readonly cache: Cache;
    readonly query: Query;
    readonly pick: Pick;
    readonly config: PanelConfig;
    readonly workspace: Workspace;
    readonly commands: Commands;
    /** Suggests a binding for one of this panel's commands. `when` defaults to
     *  this panel type; pass it explicitly to widen or narrow the scope. */
    addKeybinding(
      invocation: string,
      combo: string,
      options?: { when?: string; textInput?: boolean; focus?: string },
    ): Promise<unknown>;
    readonly fs: Fs;
    readonly history: History;
    readonly statusBar: StatusBar;
    readonly messages: Messages;
    readonly contextMenu: ContextMenu;
  }
}

/** Ergonomic alias: panels write `@param {MetafolderApi} metafolder`. */
type MetafolderApi = Metafolder.Api;
