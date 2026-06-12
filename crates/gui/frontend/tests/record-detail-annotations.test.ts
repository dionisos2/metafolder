// record-detail value annotations: the dim secondary line under reference
// values — the resolved path of a tree_ref (walked through the same field
// name on the parent chain) and the "name" field of a ref's target.

import { describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel
import { createAnnotator } from '../../panel-types/record-detail/annotations.js';

type Field = { name: string; value: { type: string; value?: unknown } };
type Entry = { uuid: string; fields: Field[] };

const treeRef = (parent: string | null, name: string) => ({
  type: 'tree_ref',
  value: { parent, name },
});

function annotatorFor(entries: Entry[]) {
  const byUuid = new Map(entries.map((e) => [e.uuid, e]));
  const getRecord = vi.fn(async (uuid: string) => {
    const entry = byUuid.get(uuid);
    if (!entry) throw new Error(`no entry ${uuid}`);
    return entry;
  });
  return { annotator: createAnnotator(getRecord), getRecord };
}

describe('tree_ref annotations', () => {
  const root: Entry = { uuid: 'r000', fields: [{ name: 'mfr_path', value: treeRef(null, '') }] };
  const dir: Entry = { uuid: 'd000', fields: [{ name: 'mfr_path', value: treeRef('r000', 'music') }] };

  test('resolves the parent chain through the same field name', async () => {
    const { annotator } = annotatorFor([root, dir]);
    const text = await annotator.annotate('mfr_path', treeRef('d000', 'song.flac'));
    expect(text).toBe('music/song.flac');
  });

  test('the root contributes an empty path segment', async () => {
    const { annotator } = annotatorFor([root]);
    expect(await annotator.annotate('mfr_path', treeRef('r000', 'top.txt'))).toBe('top.txt');
  });

  test('a rootless tree_ref needs no annotation (the name is the path)', async () => {
    const { annotator, getRecord } = annotatorFor([]);
    expect(await annotator.annotate('genre', treeRef(null, 'jazz'))).toBeNull();
    expect(getRecord).not.toHaveBeenCalled();
  });

  test('a broken chain (parent without the field) yields no annotation', async () => {
    const orphanParent: Entry = { uuid: 'p000', fields: [] };
    const { annotator } = annotatorFor([orphanParent]);
    expect(await annotator.annotate('mfr_path', treeRef('p000', 'x'))).toBeNull();
  });

  test('a missing parent entry yields no annotation instead of an error', async () => {
    const { annotator } = annotatorFor([]);
    expect(await annotator.annotate('mfr_path', treeRef('gone', 'x'))).toBeNull();
  });

  test('parent entries are fetched once across annotations', async () => {
    const { annotator, getRecord } = annotatorFor([root, dir]);
    await annotator.annotate('mfr_path', treeRef('d000', 'a.txt'));
    await annotator.annotate('mfr_path', treeRef('d000', 'b.txt'));
    expect(getRecord.mock.calls.filter(([uuid]) => uuid === 'd000')).toHaveLength(1);
  });
});

describe('ref annotations', () => {
  test('shows the target entry\'s "name" field when present', async () => {
    const target: Entry = {
      uuid: 't000',
      fields: [{ name: 'name', value: { type: 'string', value: 'Miles Davis' } }],
    };
    const { annotator } = annotatorFor([target]);
    expect(await annotator.annotate('artist', { type: 'ref', value: 't000' })).toBe('Miles Davis');
  });

  test('no "name" field, missing target, or other value types yield null', async () => {
    const bare: Entry = { uuid: 'b000', fields: [] };
    const { annotator } = annotatorFor([bare]);
    expect(await annotator.annotate('artist', { type: 'ref', value: 'b000' })).toBeNull();
    expect(await annotator.annotate('artist', { type: 'ref', value: 'gone' })).toBeNull();
    expect(await annotator.annotate('rating', { type: 'int', value: 5 })).toBeNull();
  });
});
