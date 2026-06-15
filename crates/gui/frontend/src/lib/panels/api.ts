// The `metafolder` API object handed to each panel's `mount(root, metafolder)`.
// Same surface as the former panel shim (panel-shim/shim.js) but, since panels
// now run in the shell's JS realm, every call goes straight to a Tauri command
// instead of a postMessage round-trip. One instance per mounted panel; the
// shell pushes workspace/message/visibility changes through the returned
// `push*` methods.

// @ts-expect-error plain-JS module shared with the (former) panel shim
import { createPathResolver } from '../../../../panel-shim/resolve.js';
// @ts-expect-error plain-JS module shared with the (former) panel shim
import { showMenu } from '../../../../panel-shim/menu.js';

/** A daemon proxy response (the shape `daemon_request` returns). */
interface DaemonResponse {
  status: number;
  body: unknown;
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
  root: ShadowRoot;
  visibilityGate: VisibilityGate;
}

export interface PanelApiInstance {
  /** The object passed to the panel's `mount(root, api)`. */
  api: Record<string, unknown>;
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

  function daemonRequest(method: string, path: string, body: unknown = null): Promise<DaemonResponse> {
    return benchMeasure(daemonLabel(method, path), () =>
      invoke('daemon_request', { method, path, body }),
    ) as Promise<DaemonResponse>;
  }

  // Cached GET /repos lookup (root, internal_dir, ...).
  async function repoInfo(repo: string): Promise<Record<string, unknown>> {
    if (!repoInfos.has(repo)) {
      const response = await daemonRequest('GET', '/repos');
      for (const item of (response.body as Record<string, unknown>[]) ?? []) {
        repoInfos.set(item.repo_uuid as string, item);
      }
    }
    const info = repoInfos.get(repo);
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
          return response.body; // { uuid: [paths] }
        }),
      );
    }
    return resolvers.get(repo)!;
  }

  const api: Record<string, unknown> = {
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
      parseQuery: (dsl: string) => (api.query as { parse: (d: string) => unknown }).parse(dsl),
      expandQuery: (s: string) => (api.query as { expand: (t: string) => unknown }).expand(s),
      resolvePath: (repo: string, uuid: string) => resolverFor(repo).resolveUuid(uuid),
      resolveTreeRef: (repo: string, value: unknown) => resolverFor(repo).resolveTreeRef(value),
      invalidatePath: (repo: string, uuid: string) => resolverFor(repo).invalidate(uuid),
      repoRoot: async (repo: string) => (await repoInfo(repo)).root,
      repoInternalDir: async (repo: string) => (await repoInfo(repo)).internal_dir,
      metarecordPaths: async (repo: string, metarecord: { uuid: string }) => {
        const root = (await repoInfo(repo)).root as string;
        const response = await daemonRequest('POST', `/repos/${repo}/tree/resolve`, {
          uuids: [metarecord.uuid],
        });
        const relatives = (response.body as Record<string, string[]>)?.[metarecord.uuid] ?? [];
        return relatives.map((rel) => (rel === '' ? root : `${root}/${rel}`));
      },
    },

    // Pure query transformations — run locally in the GUI backend (core).
    query: {
      parse: (dsl: string) => invoke('parse_query', { dsl }),
      expand: (text: string) => invoke('expand_query', { text }),
    },

    workspace: {
      get: (key: string) => invoke('ws_get_var', { wsId: ctx.wsId, key }),
      set: (key: string, value: unknown) => invoke('ws_set_var', { wsId: ctx.wsId, key, value }),
      adoptRepo: (repo: string) => invoke('adopt_repo', { wsId: ctx.wsId, repo }),
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
        { label, reveal, log, handler }: {
          label?: string;
          textInput?: boolean;
          reveal?: boolean;
          log?: boolean;
          handler?: (...args: string[]) => unknown;
        } = {},
      ) {
        if (handler) deps.registerHandler(name, handler);
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

    addKeybinding(invocation: string, combo: string, options: { when?: string; textInput?: boolean } = {}) {
      return invoke('suggest_keybinding', {
        combo,
        invocation,
        when: options.when === undefined ? ctx.panelType : options.when,
        textInput: options.textInput ?? false,
      });
    },

    fs: {
      readDir: (path: string) => invoke('fs_read_dir', { path }),
      stat: (path: string) => invoke('fs_stat', { path }),
    },

    statusBar: {
      message: (text: string, timeoutMs: number | null = null) =>
        invoke('post_status', { wsId: ctx.wsId, text, kind: 'info', timeoutMs }),
      error: (error: unknown, timeoutMs = 8000) =>
        invoke('post_status', {
          wsId: ctx.wsId,
          text: String((error as { message?: unknown })?.message ?? error),
          kind: 'info',
          timeoutMs,
        }),
    },

    messages: {
      list: () => invoke('get_messages', { wsId: ctx.wsId }),
      onAppend(listener: (entry: unknown) => void) {
        messageListeners.add(listener);
      },
    },
  };

  // Context menu: menus render in the shell document (showMenu appends there),
  // so viewport coordinates are correct across shadow boundaries.
  const contextMenu = (event: MouseEvent, items: unknown[]) => {
    event.preventDefault();
    event.stopPropagation();
    return showMenu(items, { x: event.clientX, y: event.clientY });
  };
  contextMenu.addDefaultItems = (provider: (event: MouseEvent) => unknown[]) =>
    deps.addDefaultMenuItems(provider);
  api.contextMenu = contextMenu;

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
