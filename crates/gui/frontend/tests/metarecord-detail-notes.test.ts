// The lines under the detail panel's head — orphaned, on an unplugged volume,
// watched or not, how busy — driven on their own, through the context the
// panel hands them. The panel-level behaviour is in
// metarecord-detail-watch-note.test.ts.

import { beforeEach, describe, expect, test } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { createNotes } from '../../default-config/panel-types/metarecord-detail/notes.js';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

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

const record = {
  uuid: 'a',
  version: 1,
  fields: [{ id: 1, name: 'mfr_path', value: { type: 'tree_ref', value: { parent: null, name: 'a' } } }],
} as Metafolder.Metarecord;

function setup({
  exists = true,
  watched = true,
  mounts = [] as unknown[],
}: { exists?: boolean; watched?: boolean; mounts?: unknown[] } = {}) {
  const shadow = shadowFor();
  let shown: Metafolder.Metarecord | null = record;
  let renders = 0;
  const daemon = {
    repoRoot: async () => '/repo',
    metarecordPaths: async () => ['/repo/a'],
    treePaths: async () => ({ a: ['/a'] }),
    call: async (_m: string, path: string, body: { paths?: string[] }) => {
      if (path.endsWith('/mounts')) return { mounts };
      if (path.endsWith('/watch/check')) {
        return { results: (body.paths ?? []).map((p) => ({ path: p, watched, reason: 'r' })) };
      }
      if (path.endsWith('/watch/activity')) return { total: 0, since_ms: 0, results: [] };
      return {};
    },
  };
  const notes = createNotes({
    root: shadow,
    daemon: daemon as never,
    exists: async () => exists,
    shown: () => shown,
    render: () => void (renders += 1),
  });
  const note = (id: string) => shadow.getElementById(id) as HTMLElement;
  return {
    notes,
    note,
    leave: () => void (shown = null),
    renders: () => renders,
  };
}

beforeEach(() => {
  document.body.innerHTML = '';
});

describe('createNotes', () => {
  test('a record whose file is gone is noted as an orphan', async () => {
    const { notes, note } = setup({ exists: false });
    notes.hide();
    await notes.fill('r');
    expect(note('orphan-note').hidden).toBe(false);
    expect(note('mount-note').hidden).toBe(true);
  });

  test('an unplugged volume is noted instead of an orphan', async () => {
    const { notes, note } = setup({
      exists: false,
      mounts: [{ uuid: 'm', path: '/a', expected: 'label:usb', current: null, state: 'offline' }],
    });
    notes.hide();
    await notes.fill('r');
    expect(note('mount-note').hidden).toBe(false);
    expect(note('orphan-note').hidden).toBe(true);
  });

  test('the watch verdict is shown either way, and repaints the panel', async () => {
    const { notes, note, renders } = setup({ watched: false });
    notes.hide();
    await notes.fill('r');
    expect(note('watch-note').hidden).toBe(false);
    expect(note('watch-note').classList.contains('unwatched')).toBe(true);
    expect(notes.needsWatch()).toBe(true);
    expect(renders()).toBeGreaterThan(0);
  });

  test('an answer for a record no longer shown is dropped', async () => {
    const { notes, note, leave } = setup({ exists: false });
    notes.hide();
    const filling = notes.fill('r');
    leave();
    await filling;
    expect(note('orphan-note').hidden).toBe(true);
    expect(note('watch-note').hidden).toBe(true);
  });
});
