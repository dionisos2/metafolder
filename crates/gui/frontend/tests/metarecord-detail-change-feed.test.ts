// metarecord-detail re-reads the displayed record when the daemon reports a
// change (doc "Daemon data in panels").
//
// The panel used to re-read only on the `metarecords:dirty` nudge, which a
// write raises only when its author remembers to: a move in an ordered list,
// `order on`, a tag answer, a script, `mf` — none of them did, and the panel
// kept showing the field as it was. The change feed reports every write,
// whoever made it.
//
// Two loads may also overlap (a change while the selection moves): the answer
// for a record no longer selected must not replace the one now shown.

import { describe, expect, test, vi } from 'vitest';
import { globalStub } from './support/global';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

const REPO = 'repo-1';
const pathOf = (uuid: string) => `/tmp/repo/${uuid}.txt`;
const tick = () => new Promise((r) => setTimeout(r, 0));

function shadowFor(): ShadowRoot {
  const html = readFileSync(resolve(PANEL_DIR, 'index.html'), 'utf8');
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

type ChangeEvent = { repo: string; uuids: string[] | null };

/** `title` values per uuid, read at each GET; `hold` delays a uuid's answer. */
async function mountPanel(
  firstUuid: string,
  titles: Map<string, string>,
  hold: Map<string, Promise<void>> = new Map(),
) {
  const noop = () => {};
  const store = new Map<string, unknown>([
    ['selected_metarecord', { uuid: firstUuid, repo: REPO }],
    ['selected_paths', [pathOf(firstUuid)]],
  ]);
  const subscriptions = new Map<string, (value: unknown) => void>();
  const menuBuilders: (() => unknown[])[] = [];
  const feed = new Set<(event: ChangeEvent) => void>();
  const reads: string[] = [];
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-detail',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    defaults: {},
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => unknown) => void fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      query: async () => ({ uuids: [], nextCursor: null, total: 0 }),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
      metarecords: async () => new Map(),
      fields: async () => [],
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string) => {
        const match = /\/metarecords\/([^/?]+)$/.exec(path);
        if (method === 'GET' && match) {
          const uuid = match[1];
          reads.push(uuid);
          const title = titles.get(uuid) ?? '';
          await hold.get(uuid);
          return {
            uuid,
            version: 1,
            fields: [{ id: 1, name: 'title', value: { type: 'string', value: title } }],
          };
        }
        return null;
      },
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async (_repo: string, metarecord: { uuid: string }) => [
        pathOf(metarecord.uuid),
      ],
    },
    changes: {
      sync: async () => {},
      subscribe: (cb: (event: ChangeEvent) => void) => {
        feed.add(cb);
        return () => feed.delete(cb);
      },
    },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: {
      refSeed: async () => null,
      labelSeparator: async () => ' | ',
    },
    recent: { touch: async () => {}, list: async () => [] },
    global: globalStub(),
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: (key: string, cb: (value: unknown) => void) => subscriptions.set(key, cb),
    },
    provide: vi.fn(),
    commands: { register: async () => {}, invoke: () => null },
    addKeybinding: async () => null,
    fs: {
      readDir: async () => [],
      stat: async () => ({}),
      exists: async () => true,
      homeDir: async () => '/home/user',
    },
    trash: {
      list: async () => [],
      restore: async () => '',
      remove: async () => {},
      empty: async () => 0,
    },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, {
      addDefaultItems: (build: () => unknown[]) => void menuBuilders.push(build),
    }),
  };
  const shadow = shadowFor();
  const mod = await import('../../default-config/panel-types/metarecord-detail/main.js');
  await mod.mount(shadow, api as never);
  await tick();
  const emit = (event: ChangeEvent) => feed.forEach((cb) => cb(event));
  return { shadow, subscriptions, reads, emit };
}

describe('metarecord-detail and the change feed', () => {
  test('a change in the repository re-reads the displayed record', async () => {
    const titles = new Map([['aaa', 'before']]);
    const { shadow, emit } = await mountPanel('aaa', titles);
    expect(shadow.textContent).toContain('before');

    // Written by somebody else (an ordered list's move, a tag answer, `mf`).
    titles.set('aaa', 'after');
    emit({ repo: REPO, uuids: ['aaa'] });
    await tick();
    await tick();

    expect(shadow.textContent).toContain('after');
    expect(shadow.textContent).not.toContain('before');
  });

  test('a change in another repository reads nothing', async () => {
    const { reads, emit } = await mountPanel('aaa', new Map([['aaa', 'x']]));
    const before = reads.length;
    emit({ repo: 'other-repo', uuids: ['aaa'] });
    await tick();
    expect(reads.length).toBe(before);
  });

  test('a late answer for the previous selection does not replace the current one', async () => {
    const titles = new Map([
      ['aaa', 'first record'],
      ['bbb', 'second record'],
    ]);
    const hold = new Map<string, Promise<void>>();
    const { shadow, subscriptions, emit } = await mountPanel('aaa', titles, hold);

    // A change starts a re-read of aaa whose answer is held back…
    let release!: () => void;
    hold.set('aaa', new Promise<void>((r) => (release = r)));
    emit({ repo: REPO, uuids: ['aaa'] });
    await tick();
    // …while the selection moves on to bbb, whose answer arrives at once.
    subscriptions.get('selected_metarecord')?.({ uuid: 'bbb', repo: REPO });
    await tick();
    await tick();
    expect(shadow.textContent).toContain('second record');

    release();
    await tick();
    await tick();
    expect(shadow.textContent).toContain('second record');
    expect(shadow.textContent).not.toContain('first record');
  });
});
