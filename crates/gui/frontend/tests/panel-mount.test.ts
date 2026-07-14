// Every panel type mounts against its own index.html.
//
// The shell hands `mount(root, metafolder)` a Shadow root holding the panel's
// markup, so a panel looking up an element that its markup does not have — or
// that has another tag than the one it assumes — is a mount-time failure. The
// shell's error boundary catches it and the panel shows an error instead of a
// UI, which no other check here would notice: tsc sees the markup as an opaque
// DOM, and no unit test loads a panel's index.html.
//
// This reproduces the shell's mount path (PanelHost.svelte) with a stub API, so
// the id/tag assumptions of every panel are checked against the real markup. It
// caught `#normal-input` being an <input> where metarecord-list assumed a
// <textarea>.

import { describe, expect, test, vi } from 'vitest';
import { readdirSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// vitest runs with cwd = crates/gui/frontend (as served-modules.test.ts notes,
// jsdom leaves `import.meta.url` unusable as a file: URL).
const PANELS_DIR = resolve(process.cwd(), '../default-config/panel-types');

const panelTypes = readdirSync(PANELS_DIR, { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name)
  .sort();

/** The shell's mount path: the panel's body, minus scripts and styles, into a Shadow root. */
function shadowFor(panelType: string): ShadowRoot {
  const html = readFileSync(resolve(PANELS_DIR, panelType, 'index.html'), 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const host = document.createElement('div');
  document.body.append(host);
  const shadow = host.attachShadow({ mode: 'open' });
  const body = document.createElement('div');
  body.className = 'mf-panel-body';
  for (const child of [...doc.body.childNodes]) {
    if (child.nodeName === 'SCRIPT' || child.nodeName === 'STYLE') continue;
    body.append(child);
  }
  shadow.append(body);
  return shadow;
}

/** A metafolder API that answers everything with an empty result. */
function stubApi(panelType: string) {
  const noop = () => {};
  return {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType,
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    visible: false,
    onVisibility: noop,
    whenVisible: noop,
    bench: { measure: (_name: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      request: async () => ({ status: 200, body: null }),
      call: async () => null,
      parseQuery: async () => null,
      expandQuery: async () => '',
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      invalidatePath: () => true,
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    cache: {
      query: async () => ({ uuids: [], nextCursor: null, total: 0 }),
      fetchMetarecords: async () => {},
      fetchTreeRefs: async () => {},
      fetchFields: async () => {},
      readMetarecord: () => null,
      readTreeRef: () => [],
      readFields: () => [],
      fieldType: () => null,
      sync: async () => {},
      REFRESH: Symbol('refresh'),
    },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: { pickerSeed: async () => null },
    workspace: {
      get: async () => null,
      set: async () => {},
      adoptRepo: async () => {},
      onChange: noop,
    },
    commands: { register: async () => null, invoke: () => null },
    addKeybinding: async () => null,
    fs: { readDir: async () => [], stat: async () => ({}), homeDir: async () => '/home/user' },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
}

describe('every panel mounts against its own markup', () => {
  // The help panel fetches its page manifest at mount; the others touch no network.
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => new Response('[]', { status: 200 })),
  );

  test.each(panelTypes)('%s', async (panelType) => {
    const mod = await import(`../../default-config/panel-types/${panelType}/main.js`);
    const cleanup = await mod.mount(shadowFor(panelType), stubApi(panelType));
    // A panel returning a cleanup function must survive being torn down.
    if (typeof cleanup === 'function') expect(() => cleanup()).not.toThrow();
  });
});
