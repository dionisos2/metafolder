// The `metafolder` API object handed to each panel's `mount(root, metafolder)`.
// Panels run in the shell's JS realm, so every call goes straight to a Tauri
// command. One instance per mounted panel; the
// shell pushes workspace/message/visibility changes through the returned
// `push*` methods.

import { createPathResolver } from '../../../../panel-shim/resolve.js';
import { showMenu } from '../../../../panel-shim/menu.js';
import { type ArgSpec, withTopLevelInvoke } from '../commands';
import { invoke as ipcInvoke } from '../ipc';
import { daemonWork } from '../working';
import { createChangeFeed, type ChangeEvent } from './changes';
import {
  createReads,
  type DaemonResponse,
  type RawFetcher,
  type ReadOptions,
} from './reads';

/** The daemon change feed — one per realm, heard by every panel. */
export const changeFeed = createChangeFeed();

let pollTimer: ReturnType<typeof setInterval> | null = null;
/**
 * Starts the background change-feed poll (GET /log/since), so panels hear of
 * the changes they did not make. Called once by the shell; not started on
 * import so unit tests stay side-effect free.
 */
export function startChangePolling(intervalMs = 7000) {
  if (pollTimer) return;
  const raw: RawFetcher = (method, path, body) =>
    ipcInvoke('daemon_request', { method, path, body });
  pollTimer = setInterval(() => {
    for (const repo of changeFeed.trackedRepos()) void changeFeed.sync(repo, raw);
  }, intervalMs);
}

/** Names each abortable daemon call, across every panel of the realm. The
 *  session prefix keeps the names apart across a reload of the WebView, which
 *  restarts the count while the proxy may still hold an abort that overtook
 *  its call (`daemon_proxy.rs`, `EARLY_ABORT_TTL`). */
const abortSession = Math.random().toString(36).slice(2, 10);
let abortSeq = 0;

/** What an aborted read rejects with — what `fetch` rejects with. */
function abortError(): DOMException {
  return new DOMException('the read was aborted', 'AbortError');
}

// The repository a daemon path reads inside (`/repos/:repo/…`), for the poll.
// A sub-path is required: `/repos/load`, `/repos/init` name no repository.
const REPO_PATH = /^\/repos\/([^/?]+)\/[^?]/;

/** The visibility gate created per panel (panel-shim/visibility.js). */
interface VisibilityGate {
  visible: boolean;
  set(visible: boolean): void;
  whenVisible(fn: () => void): void;
}

export interface PanelApiDeps {
  invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
  /** Runs a command invocation through the shell dispatcher (commands.ts). */
  dispatch: (invocation: string) => Promise<unknown>;
  /** Told a panel is about to register `name`, before its handler and
   *  arguments are recorded: the shell drops a `commands.js` entry of that
   *  name, which must not shadow a panel's command (doc "User commands"). */
  claimCommand?: (name: string) => void;
  /** Stores a panel command handler in the shell-side registry (per instance). */
  registerHandler: (name: string, handler: (...args: string[]) => unknown) => void;
  /** Stores a panel command's declared arguments, per instance like the
   *  handler: the spec's prompt/completion functions read this panel's state,
   *  and every workspace mounts its own instance under the same name. */
  registerArgs: (name: string, args: ArgSpec[]) => void;
  /** Refreshes the shell's command list after a panel registers a command. */
  onCommandsChanged: () => void;
  /** Adds a provider to the shell's single default context menu. */
  addDefaultMenuItems: (provider: (event: MouseEvent) => unknown[]) => void;
}

export interface PanelApiCtx {
  wsId: string;
  panelType: string;
  guiServer: string;
  /** Session token (doc "Session tokens") for the GUI server's protected routes. */
  sessionToken: string;
  /** Progressive-loading page size configured for this panel type, if any. */
  pageSize?: number;
  /** Shared panel UX timing knobs (config.toml `[panels]`), kebab-cased keys. */
  panelSettings?: Record<string, number>;
  /** This panel type's default values (config.toml `[panel-defaults.<type>]`),
   *  kebab-cased keys, values verbatim from the TOML. */
  panelDefaults?: Record<string, unknown>;
  root: ShadowRoot;
  visibilityGate: VisibilityGate;
}

export interface PanelApiInstance {
  /** The object passed to the panel's `mount(root, api)`. */
  api: MetafolderApi;
  /** A subscribed workspace variable changed (from `workspace-var-changed`). */
  pushVarChanged(key: string, value: unknown): void;
  /** A message-log entry was appended (null = the log was cleared). */
  pushMessageAppended(entry: unknown): void;
  /** A shell-log entry was appended (null = the log was cleared). */
  pushShellAppended(entry: unknown): void;
  /** The panel's slot visibility changed. */
  pushVisibility(visible: boolean, slot: string | null): void;
}

/** `a-b-c` → `aBC`: config tables are written in kebab-case (like the rest of
 *  `config.toml`), panels read them as JS properties. */
function camelCaseKeys(table: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(table)) {
    out[key.replace(/-([a-z])/g, (_m, c: string) => c.toUpperCase())] = value;
  }
  return out;
}

/**
 * The API a *user command* gets (doc "User commands").
 *
 * A user command has no panel: its code lives in the shell realm, and it acts
 * on whatever workspace is focused when it runs — so `wsId` is read through a
 * getter rather than captured, and the panel-only members (`panelType`,
 * `pageSize`, `defaults`, the visibility gate) are dropped rather than faked.
 * Everything else is the same object panels are handed, which is the point:
 * there is one API to learn, not two.
 *
 * `commands.register` is not offered either — a command file declares its
 * commands by exporting them, and a second way to do it would only be a way to
 * get them out of step with the file.
 */
export function createUserCommandApi(
  deps: PanelApiDeps,
  ctx: { guiServer: string; sessionToken: string; focusedWs: () => string | null },
): MetafolderApi {
  const { api } = createPanelApi(deps, {
    // Read at every call: the focused workspace is wherever the user is when
    // the command fires, not wherever they were when the file was loaded.
    get wsId() {
      return ctx.focusedWs() ?? '';
    },
    panelType: '',
    guiServer: ctx.guiServer,
    sessionToken: ctx.sessionToken,
    root: null as unknown as ShadowRoot,
    visibilityGate: {
      visible: () => true,
      whenVisible: (fn: () => unknown) => void fn(),
      onVisibility: () => {},
      set: () => {},
    } as unknown as VisibilityGate,
  });

  // The panel-only members (`panelType`, `pageSize`, `defaults`, the visibility
  // gate) are dropped rather than faked. Spread-and-delete rather than
  // omit-destructuring: pulling a method out just to discard it is a detached
  // method reference, which is exactly what `unbound-method` is about.
  const rest = { ...(api as MetafolderApi & Record<string, unknown>) };
  for (const key of ['panelType', 'pageSize', 'defaults', 'visible', 'onVisibility', 'whenVisible']) {
    delete rest[key];
  }

  return withTopLevelInvoke({
    ...rest,
    commands: {
      invoke: (invocation: string) => api.commands.invoke(invocation),
      keybindings: () => api.commands.keybindings(),
    },
    addKeybinding: (invocation: string, combo: string, options: { when?: string } = {}) =>
      // Global unless the definition says otherwise: there is no panel whose
      // focus could scope it.
      api.addKeybinding(invocation, combo, { ...options, when: options.when ?? undefined }),
  } as unknown as MetafolderApi & { commands: { invoke: (i: string) => unknown } });
}

export function createPanelApi(deps: PanelApiDeps, ctx: PanelApiCtx): PanelApiInstance {
  const { invoke } = deps;

  // Per-instance state (was module-global in the shim).
  const varListeners = new Map<string, Set<(value: unknown, key?: string) => void>>();
  const messageListeners = new Set<(entry: unknown) => void>();
  const shellListeners = new Set<(entry: Metafolder.ShellEntry | null) => void>();
  const visibilityListeners = new Set<(visible: boolean, slot: string | null) => void>();
  const resolvers = new Map<string, ReturnType<typeof createPathResolver>>();
  const repoInfos = new Map<string, Record<string, unknown>>();

  // ── Bench harness instrumentation (doc "Benchmarks") ──────────────
  function recordBench(name: string, durationMs: number) {
    void invoke('bench_record', { name, durationMs });
  }

  function benchMeasure<T>(name: string, fn: () => T): T {
    const start = performance.now();
    const finish = () => {
      const end = performance.now();
      try {
        performance.measure(name, { start, end });
      } catch {
        // User Timing L3 options unsupported here: the shell record is what
        // the harness reads, so this is non-fatal.
      }
      recordBench(name, end - start);
    };
    let result: T;
    try {
      result = fn();
    } catch (error) {
      finish();
      throw error;
    }
    if (result && typeof (result as { then?: unknown }).then === 'function') {
      return (result as unknown as Promise<unknown>).finally(finish) as unknown as T;
    }
    finish();
    return result;
  }

  // Daemon calls are auto-instrumented; ids are collapsed for low-cardinality
  // labels across a scenario.
  function daemonLabel(method: string, path: string): string {
    const norm = path.split('?')[0].replace(/\/[0-9a-f]{32}\b/g, '/:id');
    return `mf:daemon ${method} ${norm}`;
  }

  // Performs a (bench-instrumented) daemon round-trip.
  // What the user last asked this panel for, in their own words (the DSL text
  // of a query). Sent with every call the panel makes and kept in the
  // slow-operation log when one turns out to be slow: the daemon receives the
  // query IR and cannot reconstruct the text (doc "Slow log"). It is a hint for
  // a human reading the log, not a precise attribution — a call made after the
  // query carries the query's text too.
  let clientContext: string | null = null;

  const rawFetch: RawFetcher = (m, p, b, opts) => {
    const signal = opts?.signal;
    if (signal?.aborted) return Promise.reject(abortError());
    // An abortable call is named, so `daemon_abort` can drop it in flight;
    // the daemon sees the connection go and cancels the query.
    const abortId = signal ? `abort-${abortSession}-${++abortSeq}` : undefined;
    const onAbort = () => void invoke('daemon_abort', { id: abortId });
    signal?.addEventListener('abort', onAbort, { once: true });
    return daemonWork
      .track(
        benchMeasure(daemonLabel(m, p), () =>
          invoke('daemon_request', {
            method: m,
            path: p,
            body: b,
            ...(clientContext !== null && { context: clientContext }),
            ...(abortId !== undefined && { abortId }),
          }),
        ) as Promise<DaemonResponse>,
        `${m} ${p.split('?')[0]}`,
      )
      .catch((error: unknown) => {
        throw signal?.aborted ? abortError() : error;
      })
      .finally(() => signal?.removeEventListener('abort', onAbort));
  };

  // Every call goes to the daemon. A repository it addresses joins the change
  // feed's poll first — its baseline taken before this call — so the panel
  // hears of every change to what it is about to read.
  async function daemonRequest(
    method: string,
    path: string,
    body: unknown = null,
    opts?: ReadOptions,
  ): Promise<DaemonResponse> {
    const repo = path.match(REPO_PATH)?.[1];
    if (repo !== undefined) await changeFeed.baseline(repo, rawFetch);
    return rawFetch(method, path, body, opts);
  }
  const reads = createReads((m, p, b, opts) => daemonRequest(m, p, b, opts));

  // Cached GET /repos lookup (root, internal_dir, ...). UUIDs are normalized
  // (dashes stripped) so a dashed active_repo matches GET /repos' hex form.
  const normUuid = (uuid: string) => uuid.replace(/-/g, '');
  async function repoInfo(repo: string): Promise<Record<string, unknown>> {
    const key = normUuid(repo);
    if (!repoInfos.has(key)) {
      const response = await daemonRequest('GET', '/repos');
      for (const item of (response.body as Record<string, unknown>[]) ?? []) {
        repoInfos.set(normUuid(item.repo_uuid as string), item);
      }
    }
    const info = repoInfos.get(key);
    if (info === undefined) throw new Error(`repository ${repo} is not loaded`);
    return info;
  }

  function resolverFor(repo: string) {
    if (!resolvers.has(repo)) {
      resolvers.set(
        repo,
        // { uuid: [paths] }
        createPathResolver((uuids: string[], field: string) => reads.treePaths(repo, field, uuids)),
      );
    }
    return resolvers.get(repo)!;
  }

  // Shared panel timing knobs (config.toml `[panels]`), exposed as a frozen
  // camelCase object. Undefined keys fall through to each panel's own fallback.
  const raw = ctx.panelSettings ?? {};
  const panelSettings = Object.freeze({
    statusMessageMs: raw['status-message-ms'],
    statusErrorMs: raw['status-error-ms'],
    finderDebounceMs: raw['finder-debounce-ms'],
    livePreviewDebounceMs: raw['live-preview-debounce-ms'],
    taskPollMs: raw['task-poll-ms'],
  });

  // This panel type's configured defaults (config.toml
  // `[panel-defaults.<panel-type>]`), exposed as a frozen camelCase object.
  // Untyped on purpose: panel types are user-extensible, so the shell passes
  // the table through and each panel reads the keys it knows, falling back to
  // its own constant for a key the user did not configure.
  const panelDefaults = Object.freeze(camelCaseKeys(ctx.panelDefaults ?? {})) as Metafolder.Defaults;

  // Menus render in the shell document (showMenu appends there), so viewport
  // coordinates stay correct across shadow boundaries. Callable *and* carrying
  // `addDefaultItems`: an object literal cannot satisfy a call signature, hence
  // Object.assign, whose intersection type can.
  const contextMenu: Metafolder.ContextMenu = Object.assign(
    (event: MouseEvent, items: Metafolder.MenuItem[]) => {
      event.preventDefault();
      event.stopPropagation();
      // The chosen item runs its own action; nothing here awaits the choice.
      void showMenu(items, { x: event.clientX, y: event.clientY });
    },
    {
      addDefaultItems: (provider: (event: MouseEvent) => Metafolder.MenuItem[]) =>
        deps.addDefaultMenuItems(provider),
    },
  );

  const api: MetafolderApi = {
    // `mount` runs after init, so nothing to wait for; kept for compatibility.
    ready: Promise.resolve(),

    get workspaceId() {
      return ctx.wsId;
    },
    get panelType() {
      return ctx.panelType;
    },
    get guiServer() {
      return ctx.guiServer;
    },
    // Session token for the GUI server's protected routes (`/fsraw`,
    // `/thumbnail`, `/__media-probe`); appended as `?token=` to URLs loaded
    // as `<img>/<video>` src or fetched directly (doc "Session tokens").
    get sessionToken() {
      return ctx.sessionToken;
    },
    // Configured progressive-loading page size for this panel type (config.toml
    // `[page-size]`); undefined for panels without an entry.
    get pageSize() {
      return ctx.pageSize;
    },
    // Shared panel UX timing knobs (config.toml `[panels]`), as a frozen object
    // with camelCase keys. Each value may be undefined if the config is minimal,
    // so panels should read them as `metafolder.settings.xxx ?? <fallback>`.
    get settings() {
      return panelSettings;
    },
    // Configured defaults for this panel type (config.toml
    // `[panel-defaults.<panel-type>]`), as a frozen camelCase object. A key the
    // user did not configure is undefined, so panels read them as
    // `metafolder.defaults.xxx ?? <fallback>`.
    get defaults() {
      return panelDefaults;
    },

    onVisibility(listener: (visible: boolean, slot: string | null) => void) {
      visibilityListeners.add(listener);
    },
    get visible() {
      return ctx.visibilityGate.visible;
    },
    whenVisible(fn: () => void) {
      ctx.visibilityGate.whenVisible(fn);
    },

    bench: {
      measure: <T>(name: string, fn: () => T) => benchMeasure(name, fn),
      record: (name: string, durationMs: number) => recordBench(name, durationMs),
    },

    daemon: {
      // Declares what the user asked for, for the slow-operation log (see
      // `clientContext`). Cheap and local: it sets a string, it makes no call.
      setContext: (text: string | null) => {
        clientContext = text === null ? null : text.slice(0, 200);
      },
      request: (method: string, path: string, body: unknown = null, opts?: ReadOptions) =>
        daemonRequest(method, path, body, opts),
      call: async (method: string, path: string, body: unknown = null, opts?: ReadOptions) => {
        const response = await daemonRequest(method, path, body, opts);
        if (response.status >= 400) {
          const err = (response.body as { error?: string })?.error;
          throw new Error(err ?? `${method} ${path}: HTTP ${response.status}`);
        }
        return response.body;
      },
      // Creates a repository and applies its ignore preset (doc "Ignore presets"):
      // POST /repos/init then the `default` preset on the new
      // root, via core::repo_init — the same flow as `mf repo init`. Returns the
      // new repo's uuid. `daemon.call('POST', '/repos/init', ...)` would skip
      // the ignore step, so repo creation must go through here.
      initRepo: (opts: {
        root: string;
        name?: string;
        metafolder?: string;
        noIgnore?: boolean;
        ignore?: string[];
      }) =>
        invoke('repo_init', {
          root: opts.root,
          name: opts.name ?? null,
          metafolder: opts.metafolder ?? null,
          noIgnore: opts.noIgnore ?? false,
          ignore: opts.ignore ?? null,
        }) as Promise<string>,
      resolvePath: (repo: string, uuid: string) => resolverFor(repo).resolveUuid(uuid),
      resolveTreeRef: (
        repo: string,
        value: { parent: string | null; name: string },
        field = 'mfr_path',
      ) => resolverFor(repo).resolveTreeRef(value, field),
      repoRoot: async (repo: string) => (await repoInfo(repo)).root as string,
      repoInternalDir: async (repo: string) => (await repoInfo(repo)).internal_dir as string,
      metarecordPaths: async (repo: string, metarecord: { uuid: string }) => {
        const root = (await repoInfo(repo)).root as string;
        const relatives = (await reads.treePaths(repo, 'mfr_path', [metarecord.uuid]))[metarecord.uuid];
        return relatives.map((rel) => (rel === '' ? root : `${root}/${rel}`));
      },
      // Reads that return what they read (nothing is kept — lib/panels/reads.ts).
      query: (repo: string, body: Record<string, unknown>, opts?: ReadOptions) =>
        reads.query(repo, body, opts),
      metarecords: (repo: string, uuids: string[]) => reads.metarecords(repo, uuids),
      treePaths: (repo: string, field: string, uuids: string[]) => reads.treePaths(repo, field, uuids),
      fields: (repo: string) => reads.fields(repo),
    },

    // The daemon change feed (GET /log/since): what changed that this panel
    // did not write itself — a watcher-recorded rename, another panel, the CLI,
    // a rollback. `sync` polls it now (a deliberate freshness point: a refresh,
    // a catch-up after a disk change), on top of the background timer;
    // `subscribe` runs the callback with the touched uuids (`null` = the whole
    // repository) and returns the unsubscribe fn for the panel's cleanup.
    changes: {
      sync: (repo: string) => changeFeed.sync(repo, rawFetch),
      subscribe: (cb: (event: ChangeEvent) => void) => changeFeed.subscribe(cb),
    },

    // Pure query transformations — run locally in the GUI backend (core).
    query: {
      parse: (dsl: string) => invoke('parse_query', { dsl }),
      expand: (text: string) => invoke('expand_query', { text }),
      // The simplified-query grammar source as loaded at startup (help page).
      grammarSource: () => invoke('grammar_source') as Promise<string>,
    },

    // Value picker (doc "Value picker"): open a linked picker workspace
    // whose confirmed selection (a metarecord uuid) comes back as the
    // `pick_result` workspace variable, matched by `token`. `callerWs` is
    // injected so the result returns to this panel's own workspace.
    pick: {
      start: (spec: Record<string, unknown>) =>
        invoke('pick_start', { spec: { ...spec, callerWs: ctx.wsId } }) as Promise<string>,
    },

    // Read-only GUI configuration a panel may need.
    config: {
      // The `[ref-seeds]` rule naming a `ref` field's targets —
      // `{query, columns}` (doc "Ref value seeds"): which metarecords may
      // be named, and how each is shown. The named rule wins, `"*"` is the
      // default; null when neither exists.
      refSeed: (field: string) =>
        invoke('ref_seed', { field }) as Promise<{ query: string | null; columns: string } | null>,
      // What joins the columns of a multi-column completion label
      // (config.toml `[completion].label-separator`, doc "Completion
      // views").
      labelSeparator: () => invoke('label_separator') as Promise<string>,
    },

    workspace: {
      get: (key: string) => invoke('ws_get_var', { wsId: ctx.wsId, key }),
      set: (key: string, value: unknown) =>
        invoke('ws_set_var', { wsId: ctx.wsId, key, value }) as Promise<void>,
      all: () => invoke('ws_vars', { wsId: ctx.wsId }) as Promise<Record<string, unknown>>,
      adoptRepo: (repo: string) => invoke('adopt_repo', { wsId: ctx.wsId, repo }) as Promise<void>,
      onChange(key: string, listener: (value: unknown, key?: string) => void) {
        let set = varListeners.get(key);
        if (!set) {
          set = new Set();
          varListeners.set(key, set);
        }
        set.add(listener);
      },
    },

    commands: {
      register(
        name: string,
        { label, reveal, log, handler, args }: {
          label?: string;
          reveal?: boolean;
          log?: boolean;
          handler?: (...args: string[]) => unknown;
          args?: ArgSpec[];
        } = {},
      ) {
        deps.claimCommand?.(name);
        if (handler) deps.registerHandler(name, handler);
        // Declared arguments are collected interactively by the command input
        // when missing (doc "Interactive command arguments"); the spec functions stay in the
        // shell realm alongside the panel.
        if (args) deps.registerArgs(name, args);
        const result = invoke('register_command', {
          panelType: ctx.panelType,
          name,
          label: label ?? name,
          reveal: reveal ?? false,
          log: log ?? true,
        });
        deps.onCommandsChanged();
        return result;
      },
      invoke: (invocation: string) => deps.dispatch(invocation),
      // The live keybinding table, so a panel can *show* a shortcut rather than
      // restate it: the help pages fill their key hints from it (/__keyhints.js).
      keybindings: () => invoke('get_compiled_keybindings') as Promise<Metafolder.Binding[]>,
    },

    // `commands.invoke` lifted to the top of the object (doc "User
    // commands"): composing existing commands is what a command is *for*, so
    // the call it makes most should not need a path through the object. The
    // same alias the user-command API installs — one API to learn, not two.
    invoke: (invocation: string) => deps.dispatch(invocation),

    addKeybinding(
      invocation: string,
      combo: string,
      options: { when?: string; textInput?: boolean; focus?: string } = {},
    ) {
      return invoke('suggest_keybinding', {
        combo,
        invocation,
        when: options.when === undefined ? ctx.panelType : options.when,
        textInput: options.textInput ?? false,
        focus: options.focus ?? null,
      });
    },

    fs: {
      readDir: (path: string) => invoke('fs_read_dir', { path }) as Promise<Metafolder.FsEntry[]>,
      stat: (path: string) => invoke('fs_stat', { path }),
      exists: (path: string) => invoke('fs_exists', { path }) as Promise<boolean>,
      homeDir: () => invoke('fs_home_dir') as Promise<string>,
      mkdir: (path: string) => invoke('fs_mkdir', { path }) as Promise<void>,
      createFile: (path: string) => invoke('fs_create_file', { path }) as Promise<void>,
      move: (from: string, to: string) => invoke('fs_move', { from, to }) as Promise<void>,
      copy: (from: string, to: string) => invoke('fs_copy', { from, to }) as Promise<void>,
      remove: (path: string) => invoke('fs_delete', { path }) as Promise<void>,
    },

    /** Repository trash-bin (doc "Trash"): filesystem operations shared with
     *  the CLI, driven through the trash Tauri commands (no daemon endpoint). */
    trash: {
      list: (repo: string) => invoke('trash_list', { repo }) as Promise<Metafolder.TrashEntry[]>,
      restore: (repo: string, id: string) =>
        invoke('trash_restore', { repo, id }) as Promise<string>,
      remove: (repo: string, id: string) => invoke('trash_remove', { repo, id }) as Promise<void>,
      empty: (repo: string) => invoke('trash_empty', { repo }) as Promise<number>,
      trashPath: (repo: string, path: string) =>
        invoke('trash_path', { repo, path }) as Promise<string>,
      trashQuery: (repo: string, query: unknown) =>
        invoke('trash_query_metarecords', {
          wsId: ctx.wsId,
          repo,
          query,
        }) as Promise<Metafolder.BulkTrashOutcome>,
    },

    /** Coordinated navigation of the event log, files included — the shared
     *  `core::navigation` behind `mf log`, through the log Tauri commands. */
    log: {
      rollback: (repo: string, target: Record<string, unknown>) =>
        invoke('log_rollback', { repo, target }) as Promise<Metafolder.Navigated>,
      revert: (repo: string, target: Record<string, unknown>, withDependents: boolean) =>
        invoke('log_revert', { repo, target, withDependents }) as Promise<Metafolder.Reverted>,
    },

    /** Orphaned metarecords (doc "Orphans in the GUI"): the shared layer behind
     *  `mf orphan`, driven through the orphan Tauri commands. Each reports its
     *  own outcome to the status bar (`delete` also to the message log) and
     *  nudges the panels (`metarecords:dirty`), so a caller confirms in
     *  between and says nothing after. */
    orphans: {
      detect: () => invoke('orphan_detect', { wsId: ctx.wsId }) as Promise<number>,
      count: () => invoke('orphan_count', { wsId: ctx.wsId }) as Promise<number>,
      delete: () => invoke('orphan_delete', { wsId: ctx.wsId }) as Promise<number>,
    },

    /** Ignore presets (doc "Setting ignore patterns"): preset expansion reads a
     *  config file, so it goes through the backend; the eligibility/effective
     *  introspection endpoints are plain `daemon.call`s. */
    ignore: {
      presets: () =>
        invoke('ignore_presets') as Promise<
          { name: string; description: string; patterns: string[] }[]
        >,
      current: (repo: string, target: string) =>
        invoke('ignore_current', { repo, target }) as Promise<string[]>,
      apply: (repo: string, target: string, presets: string[], mode: 'add' | 'remove' | 'set') =>
        invoke('ignore_apply', { repo, target, presets, mode }) as Promise<string[]>,
      write: (repo: string, target: string, patterns: string[]) =>
        invoke('ignore_write', { repo, target, patterns }) as Promise<void>,
    },

    /** Cross-repo synchronisation (doc "Sync"): the shared `core::sync`
     *  orchestration, driven through the sync Tauri commands. `plan`/`run` run
     *  non-interactively (conflicts are left for `plan_resolve` editing). */
    sync: {
      status: (repoA: string, repoB: string) =>
        invoke('sync_status', { repoA, repoB }) as Promise<Record<string, unknown>>,
      link: (repoA: string, repoB: string, uuidA: string, uuidB: string, host?: string) =>
        invoke('sync_link', { repoA, repoB, uuidA, uuidB, host }) as Promise<{ uuid: string }>,
      unlink: (repoA: string, repoB: string, link: string, withEndpoint?: string) =>
        invoke('sync_unlink', { repoA, repoB, link, withEndpoint }) as Promise<{ uuid: string }>,
      plan: (
        repoA: string,
        repoB: string,
        intentsPath: string,
        host?: string,
        onConflict?: string,
      ) =>
        invoke('sync_plan', { repoA, repoB, intentsPath, host, onConflict }) as Promise<{
          plan_uuid: string;
          operations: number;
          warnings: string[];
        }>,
      run: (repoA: string, repoB: string) =>
        invoke('sync_run', { repoA, repoB }) as Promise<Record<string, unknown>>,
      show: (repoA: string, repoB: string, conflicts: boolean, files: boolean) =>
        invoke('sync_show', { repoA, repoB, conflicts, files }) as Promise<Record<string, unknown>>,
    },

    /** Per-repo input history (doc "Input history") — GUI-side files
     *  under `.metafolder/gui/history/<zone>`; the store behind the shared
     *  `attachHistory` helper (`/__history.js`). */
    history: {
      read: (repo: string, zone: string) =>
        invoke('history_read', { repo, zone }) as Promise<string[]>,
      append: (repo: string, zone: string, entry: string) =>
        invoke('history_append', { repo, zone, entry }) as Promise<void>,
    },

    /** Per-repo "recently viewed metarecords" — a GUI-side LRU list under
     *  `.metafolder/gui/recent` (crate::recent). `touch` records a view (the
     *  timestamp is the GUI's clock); `list` returns entries newest first. */
    recent: {
      list: (repo: string, limit?: number) =>
        invoke('recent_read', { repo, limit }) as Promise<{ uuid: string; viewed_at: string }[]>,
      touch: (repo: string, uuid: string) =>
        invoke('recent_touch', { repo, uuid }) as Promise<void>,
    },

    statusBar: {
      message: (text: string, timeoutMs: number | null = null) =>
        invoke('post_status', { wsId: ctx.wsId, text, kind: 'info', timeoutMs }) as Promise<void>,
      error: (error: unknown, timeoutMs = 8000) =>
        invoke('post_status', {
          wsId: ctx.wsId,
          text: String((error as { message?: unknown })?.message ?? error),
          kind: 'error',
          timeoutMs,
        }) as Promise<void>,
    },

    messages: {
      list: () => invoke('get_messages', { wsId: ctx.wsId }) as Promise<unknown[]>,
      /** Appends a line to this workspace's persistent message log. */
      append: (text: string) =>
        invoke('append_message', { wsId: ctx.wsId, text }) as Promise<void>,
      onAppend(listener: (entry: unknown) => void) {
        messageListeners.add(listener);
      },
    },

    /** What the shell lines run in this workspace printed (doc "shell panel"). */
    shell: {
      list: () => invoke('get_shell_log', { wsId: ctx.wsId }) as Promise<Metafolder.ShellEntry[]>,
      onAppend(listener: (entry: Metafolder.ShellEntry | null) => void) {
        shellListeners.add(listener);
      },
    },
    contextMenu,
  };

  return {
    api,
    pushVarChanged(key, value) {
      for (const l of varListeners.get(key) ?? []) l(value);
      for (const l of varListeners.get('*') ?? []) l(value, key);
    },
    pushMessageAppended(entry) {
      for (const l of messageListeners) l(entry);
    },
    pushShellAppended(entry) {
      for (const l of shellListeners) l(entry as Metafolder.ShellEntry | null);
    },
    pushVisibility(visible, slot) {
      ctx.visibilityGate.set(visible);
      for (const l of visibilityListeners) l(visible, slot);
    },
  };
}
