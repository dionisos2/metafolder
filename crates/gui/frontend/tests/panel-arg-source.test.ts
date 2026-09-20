// Where a panel command's declared arguments live (lib/panels/argSource.ts):
// per mounted instance — one per workspace × panel type — and answered for the
// focused workspace, so the minibuffer's prompts and the handler that receives
// them are the same panel.

import { describe, expect, test } from 'vitest';
import type { ArgSpec } from '../src/lib/commands';
import { createPanelArgSource, type PanelArgSourceDeps } from '../src/lib/panels/argSource';

const spec = (name: string): ArgSpec => ({ name, prompt: () => `${name}?` });

function setup(overrides: Partial<PanelArgSourceDeps> = {}) {
  const mounted: string[] = [];
  let focused: string | null = 'ws-1';
  const owners = new Map<string, string>([['metarecord:bulk', 'metarecord-detail']]);
  const source = createPanelArgSource({
    focusedWs: () => focused,
    ownerOf: (name) => owners.get(name),
    ensureMounted: async (wsId, panelType) => void mounted.push(`${wsId}|${panelType}`),
    ...overrides,
  });
  return { source, mounted, focus: (ws: string | null) => (focused = ws) };
}

describe('resolve', () => {
  test('answers with the focused workspace instance, not the last registered', () => {
    const { source, focus } = setup();
    const mine = [spec('field')];
    const theirs = [spec('field')];
    source.register('ws-1|metarecord-detail', 'metarecord:bulk', mine);
    // A second workspace mounts the same panel type afterwards: under a
    // name-keyed registry this is the spec everyone would have got.
    source.register('ws-2|metarecord-detail', 'metarecord:bulk', theirs);
    expect(source.resolve('metarecord:bulk')).toBe(mine);
    focus('ws-2');
    expect(source.resolve('metarecord:bulk')).toBe(theirs);
  });

  test('a workspace without the panel mounted has no spec of its own', () => {
    const { source, focus } = setup();
    source.register('ws-1|metarecord-detail', 'metarecord:bulk', [spec('field')]);
    focus('ws-3');
    expect(source.resolve('metarecord:bulk')).toBeUndefined();
  });

  test('a workspace id that merely prefixes another is not matched', () => {
    const { source, focus } = setup();
    source.register('ws-10|metarecord-detail', 'metarecord:bulk', [spec('field')]);
    focus('ws-1');
    expect(source.resolve('metarecord:bulk')).toBeUndefined();
  });

  test('an unknown command, and no focused workspace, resolve to nothing', () => {
    const { source, focus } = setup();
    source.register('ws-1|metarecord-detail', 'metarecord:bulk', [spec('field')]);
    expect(source.resolve('metarecord:field')).toBeUndefined();
    focus(null);
    expect(source.resolve('metarecord:bulk')).toBeUndefined();
  });

  test('forget drops that instance only', () => {
    const { source, focus } = setup();
    source.register('ws-1|metarecord-detail', 'metarecord:bulk', [spec('a')]);
    source.register('ws-1|metarecord-list', 'metarecord-list:apply', [spec('b')]);
    source.register('ws-2|metarecord-detail', 'metarecord:bulk', [spec('c')]);
    source.forget('ws-1|metarecord-detail');
    expect(source.resolve('metarecord:bulk')).toBeUndefined();
    expect(source.resolve('metarecord-list:apply')?.[0].name).toBe('b');
    focus('ws-2');
    expect(source.resolve('metarecord:bulk')?.[0].name).toBe('c');
  });
});

describe('prepare', () => {
  test('mounts the focused workspace instance of the owning panel', async () => {
    const { source, mounted } = setup();
    await source.prepare('metarecord:bulk');
    expect(mounted).toEqual(['ws-1|metarecord-detail']);
  });

  test('a shell builtin owns no panel and mounts nothing', async () => {
    const { source, mounted } = setup();
    await source.prepare('panel:set');
    expect(mounted).toEqual([]);
  });

  test('nothing is mounted without a focused workspace', async () => {
    const { source, mounted, focus } = setup();
    focus(null);
    await source.prepare('metarecord:bulk');
    expect(mounted).toEqual([]);
  });
});
