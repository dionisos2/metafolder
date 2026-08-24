// The `metafolder` API object handed to each panel's `mount(root, metafolder)`.
// Same surface as the former panel shim (panel-shim/shim.js) but, since panels
// now run in the shell's JS realm, every call goes straight to a Tauri command
// instead of a postMessage round-trip. One instance per mounted panel; the
// shell pushes workspace/message/visibility changes through the returned
// `push*` methods.

import { createPathResolver } from '../../../../panel-shim/resolve.js';
import { showMenu } from '../../../../panel-shim/menu.js';
import { type ArgSpec, registerArgs } from '../commands';
import { invoke as ipcInvoke } from '../ipc';
import { createCache, type DaemonResponse, type RawFetcher } from './cache';

/** The shared daemon-data cache — one per realm, read by every panel. */
export const sharedCache = createCache();

let pollTimer: ReturnType<typeof setInterval> | null = null;
/**
 * Starts the background change-feed poll (GET /log/since) that keeps the cache
 * fresh. Called once by the shell; not started on import so unit tests stay
 * side-effect free.
 */
export function startCachePolling(intervalMs = 7000) {
  if (pollTimer) return;
  const raw: RawFetcher = (method, path, body) =>
    ipcInvoke('daemon_request', { method, path, body });
  pollTimer = setInterval(() => {
    for (const repo of sharedCache.trackedRepos()) void sharedCache.sync(repo, raw);
  }, intervalMs);
}

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
  /** Stores a panel command handler in the shell-side registry (per instance). */
  registerHandler: (name: string, handler: (...args: string[]) => unknown) => void;
  /** Refreshes the shell's command list after a panel registers a command. */
  onCommandsChanged: () => void;
  /** Adds a provider to the shell's single default context menu. */
  addDefaultMenuItems: (provider: (event: MouseEvent) => unknown[]) => void;
}

export interface PanelApiCtx {
  wsId: string;
  panelType: string;
  guiServer: string;
  /** Session token (spec-auth) for the GUI server's protected routes. */
  sessionToken: string;
  /** Progressive-loading page size configured for this panel type, if any. */
  pageSize?: number;
  /** Shared panel UX timing knobs (config.toml `[panels]`), kebab-cased keys. */
  panelSettings?: Record<string, number>;
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
  /** The panel's slot visibility changed. */
  pushVisibility(visible: boolean, slot: string | null): void;
}

export function createPanelApi(deps: PanelApiDeps, ctx: PanelApiCtx): PanelApiInstance {
  const { invoke } = deps;

  // Per-instance state (was module-global in the shim).
  const varListeners = new Map<string, Set<(value: unknown, key?: string) => void>>();
  const messageListeners = new Set<(entry: unknown) => void>();
  const visibilityListeners = new Set<(visible: boolean, slot: string | null) => void>();
  const resolvers = new Map<string, ReturnType<typeof createPathResolver>>();
  const repoInfos = new Map<string, Record<string, unknown>>();

  // ── Bench harness instrumentation (spec-gui "Bench harness") ──────────────
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

  // Performs a real (bench-instrumented) daemon round-trip — the cache's miss
  // path. Cache hits never reach here, so they cost nothing and record nothing.
  const rawFetch: RawFetcher = (m, p, b) =>
    benchMeasure(daemonLabel(m, p), () =>
      invoke('daemon_request', { method: m, path: p, body: b }),
    ) as Promise<DaemonResponse>;

  function daemonRequest(method: string, path: string, body: unknown = null): Promise<DaemonResponse> {
    return sharedCache.request(method, path, body, rawFetch);
  }

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
        createPathResolver(async (uuids: string[]) => {
          const response = await daemonRequest('POST', `/repos/${repo}/tree/resolve`, { uuids });
          if (response.status !== 200) {
            const err = (response.body as { error?: string })?.error;
            throw new Error(err ?? `tree/resolve failed (HTTP ${response.status})`);
          }
          return response.body as Record<string, string[]>; // { uuid: [paths] }
        }),
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
    // as `<img>/<video>` src or fetched directly (spec-auth).
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
      request: (method: string, path: string, body: unknown = null) =>
        daemonRequest(method, path, body),
      call: async (method: string, path: string, body: unknown = null) => {
        const response = await daemonRequest(method, path, body);
        if (response.status >= 400) {
          const err = (response.body as { error?: string })?.error;
          throw new Error(err ?? `${method} ${path}: HTTP ${response.status}`);
        }
        return response.body;
      },
      // Creates a repository and applies its ignore preset (spec-file-tracking
      // "Ignore presets"): POST /repos/init then the `default` preset on the new
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
      parseQuery: (dsl: string) => api.query.parse(dsl),
      expandQuery: (s: string) => api.query.expand(s),
      resolvePath: (repo: string, uuid: string) => resolverFor(repo).resolveUuid(uuid),
      resolveTreeRef: (repo: string, value: { parent: string | null; name: string }) =>
        resolverFor(repo).resolveTreeRef(value),
      invalidatePath: (repo: string, uuid: string) => resolverFor(repo).invalidate(uuid),
      repoRoot: async (repo: string) => (await repoInfo(repo)).root as string,
      repoInternalDir: async (repo: string) => (await repoInfo(repo)).internal_dir as string,
      metarecordPaths: async (repo: string, metarecord: { uuid: string }) => {
        const root = (await repoInfo(repo)).root as string;
        const response = await daemonRequest('POST', `/repos/${repo}/tree/resolve`, {
          uuids: [metarecord.uuid],
        });
        const relatives = (response.body as Record<string, string[]>)?.[metarecord.uuid] ?? [];
        return relatives.map((rel) => (rel === '' ? root : `${root}/${rel}`));
      },
    },

    // Shared daemon-data cache: fetch (async, populates) then read (sync, for
    // render). `read*` returns `cache.REFRESH` when a datum is absent — the
    // panel renders a placeholder and schedules a refresh. Cached data is
    // read-only (never mutate it).
    cache: {
      query: (repo: string, body: Record<string, unknown>) => sharedCache.query(repo, body, rawFetch),
      fetchMetarecords: (repo: string, uuids: string[]) =>
        sharedCache.fetchMetarecords(repo, uuids, rawFetch),
      fetchTreeRefs: (repo: string, field: string, uuids: string[]) =>
        sharedCache.fetchTreeRefs(repo, field, uuids, rawFetch),
      fetchFields: (repo: string) => sharedCache.fetchFields(repo, rawFetch),
      readMetarecord: (repo: string, uuid: string) => sharedCache.readMetarecord(repo, uuid),
      readTreeRef: (repo: string, field: string, uuid: string) =>
        sharedCache.readTreeRef(repo, field, uuid),
      readFields: (repo: string) => sharedCache.readFields(repo),
      fieldType: (repo: string, name: string) => sharedCache.fieldType(repo, name),
      // Poll the change feed now (a deliberate freshness point: a query, a
      // refresh, a panel becoming visible) — on top of the background timer.
      sync: (repo: string) => sharedCache.sync(repo, rawFetch),
      // Subscribe to feed-driven changes (a watcher-reflected write, a
      // rollback…): the callback runs with the touched uuids (`null` = a coarse
      // whole-repo refresh) so a panel can re-render its displayed rows. Returns
      // an unsubscribe fn — call it in the panel's cleanup.
      subscribe: (cb: (event: import('./cache').ChangeEvent) => void) => sharedCache.subscribe(cb),
      REFRESH: sharedCache.REFRESH,
    },

    // Pure query transformations — run locally in the GUI backend (core).
    query: {
      parse: (dsl: string) => invoke('parse_query', { dsl }),
      expand: (text: string) => invoke('expand_query', { text }),
      // The simplified-query grammar source as loaded at startup (help page).
      grammarSource: () => invoke('grammar_source') as Promise<string>,
    },

    // Value picker (spec-gui "Value picker"): open a linked picker workspace
    // whose confirmed selection (a metarecord uuid) comes back as the
    // `pick_result` workspace variable, matched by `token`. `callerWs` is
    // injected so the result returns to this panel's own workspace.
    pick: {
      start: (spec: Record<string, unknown>) =>
        invoke('pick_start', { spec: { ...spec, callerWs: ctx.wsId } }) as Promise<string>,
    },

    // Read-only GUI configuration a panel may need.
    config: {
      // Configured `ref` picker seed query for a field name, or null
      // (config.toml `[picker-seeds]`).
      pickerSeed: (field: string) => invoke('picker_seed', { field }) as Promise<string | null>,
      // Configured `ref` value completion seed (a tree_ref field name) for a
      // field name, or null (config.toml `[ref-completion-seeds]`).
      refCompletionSeed: (field: string) =>
        invoke('ref_completion_seed', { field }) as Promise<string | null>,
    },

    workspace: {
      get: (key: string) => invoke('ws_get_var', { wsId: ctx.wsId, key }),
      set: (key: string, value: unknown) =>
        invoke('ws_set_var', { wsId: ctx.wsId, key, value }) as Promise<void>,
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
          textInput?: boolean;
          reveal?: boolean;
          log?: boolean;
          handler?: (...args: string[]) => unknown;
          args?: ArgSpec[];
        } = {},
      ) {
        if (handler) deps.registerHandler(name, handler);
        // Declared arguments are collected interactively by the command input
        // when missing (spec-gui "Command"); the spec functions stay in the
        // shell realm alongside the panel.
        if (args) registerArgs(name, args);
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
    },

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
      homeDir: () => invoke('fs_home_dir') as Promise<string>,
      mkdir: (path: string) => invoke('fs_mkdir', { path }) as Promise<void>,
      createFile: (path: string) => invoke('fs_create_file', { path }) as Promise<void>,
      move: (from: string, to: string) => invoke('fs_move', { from, to }) as Promise<void>,
      copy: (from: string, to: string) => invoke('fs_copy', { from, to }) as Promise<void>,
      remove: (path: string) => invoke('fs_delete', { path }) as Promise<void>,
    },

    /** Repository trash-bin (spec-trash.org): filesystem operations shared with
     *  the CLI, driven through the trash Tauri commands (no daemon endpoint). */
    trash: {
      list: (repo: string) => invoke('trash_list', { repo }) as Promise<Metafolder.TrashEntry[]>,
      restore: (repo: string, id: string) =>
        invoke('trash_restore', { repo, id }) as Promise<string>,
      remove: (repo: string, id: string) => invoke('trash_remove', { repo, id }) as Promise<void>,
      empty: (repo: string) => invoke('trash_empty', { repo }) as Promise<number>,
      trashPath: (repo: string, path: string) =>
        invoke('trash_path', { repo, path }) as Promise<string>,
    },

    /** Cross-repo synchronisation (spec-sync): the shared `core::sync`
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

    /** Per-repo input history (spec-gui "Input history") — GUI-side files
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
    pushVisibility(visible, slot) {
      ctx.visibilityGate.set(visible);
      for (const l of visibilityListeners) l(visible, slot);
    },
  };
}
