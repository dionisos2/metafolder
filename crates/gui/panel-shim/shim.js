// metafolder panel shim — injected as the first <script type="module"> of
// every panel-type document by the GUI server. Provides window.metafolder
// (spec-gui "The metafolder API") and bridges to the shell via postMessage.
// Module execution is in document order, so this runs before the panel's
// own module scripts: window.metafolder is always defined for them.

import { comboFromEvent, createMatcher } from '/__keymatch.js';
import { createPathResolver } from '/__resolve.js';
import {
  hasOpenMenu,
  installContextMenuSuppression,
  installDefaultContextMenu,
  showMenu,
} from '/__menu.js';

const pendingRequests = new Map();
let nextRequestId = 1;

const varListeners = new Map(); // key -> Set<fn>
const commandHandlers = new Map(); // name -> fn
const visibilityListeners = new Set();
const messageListeners = new Set(); // message-log appends (null = cleared)

let initData = null;
let resolveReady;
const ready = new Promise((resolve) => (resolveReady = resolve));

const matcher = createMatcher([]);

// Per-repo TreeRef path resolvers and repo-info cache.
const resolvers = new Map();
const repoInfos = new Map();

// Cached GET /repos lookup (root, internal_dir, ...).
async function repoInfo(repo) {
  if (!repoInfos.has(repo)) {
    const response = await request('daemon.request', {
      method: 'GET',
      path: '/repos',
      body: null,
    });
    for (const item of response.body ?? []) repoInfos.set(item.repo_uuid, item);
  }
  const info = repoInfos.get(repo);
  if (info === undefined) throw new Error(`repository ${repo} is not loaded`);
  return info;
}

function resolverFor(repo) {
  if (!resolvers.has(repo)) {
    resolvers.set(
      repo,
      createPathResolver(async (uuid) => {
        const response = await request('daemon.request', {
          method: 'GET',
          path: `/repos/${repo}/records/${uuid}`,
          body: null,
        });
        if (response.status !== 200) {
          throw new Error(response.body?.error ?? `entry ${uuid} not found`);
        }
        return response.body;
      }),
    );
  }
  return resolvers.get(repo);
}

function send(message) {
  window.parent.postMessage({ mf: true, ...message }, '*');
}

function request(method, params) {
  return new Promise((resolve, reject) => {
    const id = nextRequestId++;
    pendingRequests.set(id, { resolve, reject });
    send({ type: 'request', id, method, params });
  });
}

window.addEventListener('message', (event) => {
  if (event.source !== window.parent) return;
  const message = event.data;
  if (!message || message.mf !== true) return;

  switch (message.type) {
    case 'init': {
      initData = message;
      matcher.setBindings(message.keytable ?? []);
      clearInterval(readyRetry);
      resolveReady();
      break;
    }
    case 'response': {
      const pending = pendingRequests.get(message.id);
      if (pending) {
        pendingRequests.delete(message.id);
        if (message.ok) pending.resolve(message.result);
        else pending.reject(new Error(message.error));
      }
      break;
    }
    case 'var-changed': {
      for (const listener of varListeners.get(message.key) ?? []) listener(message.value);
      for (const listener of varListeners.get('*') ?? []) listener(message.value, message.key);
      break;
    }
    case 'command': {
      const handler = commandHandlers.get(message.name);
      Promise.resolve()
        .then(() => {
          if (!handler) throw new Error(`no handler for ${message.name}`);
          return handler(...message.args);
        })
        .then(
          () => send({ type: 'command-result', invocationId: message.invocationId, ok: true }),
          (error) =>
            send({
              type: 'command-result',
              invocationId: message.invocationId,
              ok: false,
              error: String(error),
            }),
        );
      break;
    }
    case 'message-appended':
      for (const listener of messageListeners) listener(message.entry);
      break;
    case 'keytable':
      matcher.setBindings(message.bindings ?? []);
      break;
    case 'visibility':
      for (const listener of visibilityListeners) listener(message.visible, message.slot);
      break;
  }
});

// The editing:* commands act on THIS document's focused text input; they
// are handled here, not forwarded to the shell. `null` means "keep the
// native behaviour": Enter must still submit panel forms and reach the
// panel's own keydown handlers.
const EDITING_ACTIONS = {
  'editing:confirm': null,
  'editing:unfocus': () => document.activeElement?.blur(),
  'editing:goto-line-start': () => document.activeElement?.setSelectionRange?.(0, 0),
  'editing:goto-line-end': () => {
    const active = document.activeElement;
    const end = active?.value?.length ?? 0;
    active?.setSelectionRange?.(end, end);
  },
};

// The native context menu is suppressed (spec-gui "Context menus");
// panels show HTML menus via metafolder.contextMenu instead, and the
// default menu (Copy + layout commands) answers everywhere else. Element
// handlers calling metafolder.contextMenu stop propagation, so they
// always win over the default menu.
installContextMenuSuppression(window);
const defaultMenu = installDefaultContextMenu(window, (invocation) =>
  // Dispatch errors already reach the status bar through the shell.
  request('commands.invoke', { invocation }).catch(() => {}),
);

// Key events never cross the iframe boundary: run the shared matcher here
// and forward resolved invocations to the shell.
window.addEventListener(
  'keydown',
  (event) => {
    if (hasOpenMenu()) return; // the menu's own navigation handles the keys
    const combo = comboFromEvent(event);
    if (!combo || !initData) return;
    const active = document.activeElement;
    const textInput =
      !!active &&
      (active.tagName === 'INPUT' ||
        active.tagName === 'TEXTAREA' ||
        active.tagName === 'SELECT' ||
        active.isContentEditable);
    const result = matcher.feed(combo, { panelType: initData.panelType, textInput });
    if (!result) return;
    if (result.invocation && result.invocation in EDITING_ACTIONS) {
      const action = EDITING_ACTIONS[result.invocation];
      if (action) {
        event.preventDefault();
        event.stopPropagation();
        action();
      }
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    if (result.invocation) send({ type: 'key-resolved', invocation: result.invocation });
  },
  { capture: true },
);

// Clicking into a panel focuses its slot.
window.addEventListener('focus', () => send({ type: 'focused' }));

window.metafolder = {
  ready,

  get workspaceId() {
    return initData?.workspaceId ?? null;
  },
  get panelType() {
    return initData?.panelType ?? null;
  },
  get guiServer() {
    return initData?.guiServer ?? '';
  },

  onVisibility(listener) {
    visibilityListeners.add(listener);
  },

  daemon: {
    /** Raw daemon call: request('GET', '/repos') etc. */
    request: (method, path, body = null) => request('daemon.request', { method, path, body }),
    /** Like request, but throws on >= 400 and returns the body directly. */
    call: async (method, path, body = null) => {
      const response = await request('daemon.request', { method, path, body });
      if (response.status >= 400) {
        throw new Error(response.body?.error ?? `${method} ${path}: HTTP ${response.status}`);
      }
      return response.body;
    },
    /** Compiles a query DSL string to the Query JSON IR. */
    parseQuery: (dsl) => request('daemon.parseQuery', { dsl }),
    /** Repo-root-relative path of an entry (lazy walk, memoized). */
    resolvePath: (repo, uuid) => resolverFor(repo).resolveUuid(uuid),
    /** Same, for a raw tree_ref value {parent, name}. */
    resolveTreeRef: (repo, value) => resolverFor(repo).resolveTreeRef(value),
    /** Drops the cached path of an entry (after a move/rename). */
    invalidatePath: (repo, uuid) => resolverFor(repo).invalidate(uuid),
    /** Absolute root directory of a repository (cached GET /repos). */
    repoRoot: async (repo) => (await repoInfo(repo)).root,
    /**
     * Absolute path of the repository's `.metafolder/internal/` directory
     * (cached GET /repos) — the only path always excluded from tracking.
     */
    repoInternalDir: async (repo) => (await repoInfo(repo)).internal_dir,
    /**
     * Absolute filesystem paths of an record: one per `mfr_path` tree_ref
     * (fields are a multi-map), unresolvable (stale) refs skipped.
     */
    recordPaths: async (repo, record) => {
      const root = await window.metafolder.daemon.repoRoot(repo);
      const paths = [];
      for (const f of record.fields ?? []) {
        if (f.name !== 'mfr_path' || f.value.type !== 'tree_ref') continue;
        try {
          const relative = await resolverFor(repo).resolveTreeRef(f.value.value);
          paths.push(relative === '' ? root : `${root}/${relative}`);
        } catch {
          /* stale TreeRef: skip */
        }
      }
      return paths;
    },
  },

  workspace: {
    get: (key) => request('workspace.get', { key }),
    set: (key, value) => request('workspace.set', { key, value }),
    /** Sets active_repo on a workspace that has none yet (repos panel). */
    adoptRepo: (repo) => request('workspace.adoptRepo', { repo }),
    onChange(key, listener) {
      if (!varListeners.has(key)) {
        varListeners.set(key, new Set());
        void request('workspace.subscribe', { key });
      }
      varListeners.get(key).add(listener);
    },
  },

  commands: {
    register(name, { label, scope, textInput, reveal, handler } = {}) {
      if (handler) commandHandlers.set(name, handler);
      return request('commands.register', {
        name,
        label: label ?? name,
        scope: scope ?? null,
        textInput: textInput ?? false,
        reveal: reveal ?? false,
      });
    },
    invoke: (invocation) => request('commands.invoke', { invocation }),
  },

  addKeybinding(invocation, combo, options = {}) {
    return request('addKeybinding', {
      invocation,
      combo,
      when: options.when,
      textInput: options.textInput ?? false,
    });
  },

  fs: {
    readDir: (path) => request('fs.readDir', { path }),
    stat: (path) => request('fs.stat', { path }),
  },

  /**
   * Shows an HTML context menu at the event's position. `items` is an
   * array of {label, action?, disabled?} and '-' separators (/__menu.js);
   * resolves with the chosen item (its action already run) or null.
   */
  contextMenu(event, items) {
    event.preventDefault();
    event.stopPropagation();
    return showMenu(items, { x: event.clientX, y: event.clientY });
  },

  statusBar: {
    message: (text, timeoutMs = null) => request('statusBar.message', { text, timeoutMs }),
    /** Standard error report: statusBar.error(error) in a catch block. */
    error: (error, timeoutMs = 8000) =>
      request('statusBar.message', { text: String(error?.message ?? error), timeoutMs }),
  },

  messages: {
    list: () => request('messages.list', {}),
    /** entry is null when the log was cleared. */
    onAppend(listener) {
      if (messageListeners.size === 0) void request('workspace.subscribe', { key: 'messages' });
      messageListeners.add(listener);
    },
  },
};

/** Registers a default-menu item provider (`event => items`); the items
 *  appear above the built-in entries (spec-gui "Context menus"). */
window.metafolder.contextMenu.addDefaultItems = (provider) => defaultMenu.addItems(provider);

// Announce until the shell answers with init: the very first message can
// race the WebView's cross-origin WindowProxy swap (the shell cannot
// match event.source against the iframe yet).
send({ type: 'ready' });
const readyRetry = setInterval(() => {
  if (initData === null) send({ type: 'ready' });
  else clearInterval(readyRetry);
}, 200);
