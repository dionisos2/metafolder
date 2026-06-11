// metafolder panel shim — injected as the first <script type="module"> of
// every panel-type document by the GUI server. Provides window.metafolder
// (spec-gui "The metafolder API") and bridges to the shell via postMessage.
// Module execution is in document order, so this runs before the panel's
// own module scripts: window.metafolder is always defined for them.

import { comboFromEvent, createMatcher } from '/__keymatch.js';
import { createPathResolver } from '/__resolve.js';

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

// Per-repo TreeRef path resolvers and repo-root cache.
const resolvers = new Map();
const repoRoots = new Map();

function resolverFor(repo) {
  if (!resolvers.has(repo)) {
    resolvers.set(
      repo,
      createPathResolver(async (uuid) => {
        const response = await request('daemon.request', {
          method: 'GET',
          path: `/repos/${repo}/metadata/${uuid}`,
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

// Key events never cross the iframe boundary: run the shared matcher here
// and forward resolved invocations to the shell.
window.addEventListener(
  'keydown',
  (event) => {
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
    if (result) {
      event.preventDefault();
      event.stopPropagation();
      if (result.invocation) send({ type: 'key-resolved', invocation: result.invocation });
    }
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
    /** Compiles a query DSL string to the Query JSON IR. */
    parseQuery: (dsl) => request('daemon.parseQuery', { dsl }),
    /** Repo-root-relative path of an entry (lazy walk, memoized). */
    resolvePath: (repo, uuid) => resolverFor(repo).resolveUuid(uuid),
    /** Same, for a raw tree_ref value {parent, name}. */
    resolveTreeRef: (repo, value) => resolverFor(repo).resolveTreeRef(value),
    /** Drops the cached path of an entry (after a move/rename). */
    invalidatePath: (repo, uuid) => resolverFor(repo).invalidate(uuid),
    /** Absolute root directory of a repository (cached GET /repos). */
    repoRoot: async (repo) => {
      if (!repoRoots.has(repo)) {
        const response = await request('daemon.request', {
          method: 'GET',
          path: '/repos',
          body: null,
        });
        for (const item of response.body ?? []) repoRoots.set(item.repo_uuid, item.root);
      }
      const root = repoRoots.get(repo);
      if (root === undefined) throw new Error(`repository ${repo} is not loaded`);
      return root;
    },
  },

  workspace: {
    get: (key) => request('workspace.get', { key }),
    set: (key, value) => request('workspace.set', { key, value }),
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

  statusBar: {
    message: (text, timeoutMs = null) => request('statusBar.message', { text, timeoutMs }),
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

// Announce until the shell answers with init: the very first message can
// race the WebView's cross-origin WindowProxy swap (the shell cannot
// match event.source against the iframe yet).
send({ type: 'ready' });
const readyRetry = setInterval(() => {
  if (initData === null) send({ type: 'ready' });
  else clearInterval(readyRetry);
}, 200);
