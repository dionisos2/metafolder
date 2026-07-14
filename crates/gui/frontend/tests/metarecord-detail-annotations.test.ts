// metarecord-detail value annotations: the dim secondary line under reference
// values — the resolved path of a tree_ref (via the daemon's tree-resolve
// endpoint, general over the field) and the "name" field of a ref's target.

import { describe, expect, test, vi } from 'vitest';
import { createAnnotator } from '../../default-config/panel-types/metarecord-detail/annotations.js';

type Entry = Metafolder.Metarecord & { fields: Metafolder.Field[] };

const treeRef = (parent: string | null, name: string): Metafolder.Value => ({
  type: 'tree_ref',
  value: { parent, name },
});

function annotatorFor(entries: Entry[]) {
  const byUuid = new Map(entries.map((e) => [e.uuid, e]));

  // Simulate the daemon tree-resolve endpoint: walk `field`'s parent chain to a
  // root-relative path (no leading slash; the root's empty name drops out).
  function pathOf(field: string, uuid: string): string | null {
    const components: string[] = [];
    let cur: string | null = uuid;
    while (cur) {
      // Annotated: `cur` is re-assigned from `f`, so inference would be circular.
      const f: Metafolder.Field | undefined = byUuid
        .get(cur)
        ?.fields.find((x) => x.name === field && x.value.type === 'tree_ref');
      if (f?.value.type !== 'tree_ref') return null;
      const { parent, name }: Metafolder.TreeRef = f.value.value;
      components.push(name);
      cur = parent;
    }
    return components.reverse().filter(Boolean).join('/');
  }

  const resolvePaths = vi.fn(async (field: string, uuids: string[]) => {
    const out: Record<string, string[]> = {};
    for (const u of uuids) {
      const p = pathOf(field, u);
      out[u] = p === null ? [] : [p];
    }
    return out;
  });
  const getMetarecords = vi.fn(async (uuids: string[]) => {
    const out: Record<string, Entry> = {};
    for (const u of uuids) {
      const e = byUuid.get(u);
      if (e) out[u] = e;
    }
    return out;
  });
  return { annotator: createAnnotator({ resolvePaths, getMetarecords }), resolvePaths, getMetarecords };
}

describe('tree_ref annotations', () => {
  const root: Entry = { uuid: 'r000', fields: [{ name: 'mfr_path', value: treeRef(null, '') }] };
  const dir: Entry = { uuid: 'd000', fields: [{ name: 'mfr_path', value: treeRef('r000', 'music') }] };

  test('resolves the path through the daemon endpoint', async () => {
    const { annotator } = annotatorFor([root, dir]);
    expect(await annotator.annotate('mfr_path', treeRef('d000', 'song.flac'))).toBe('music/song.flac');
  });

  test('resolves through the endpoint (no client-side chain walk)', async () => {
    const { annotator, resolvePaths } = annotatorFor([root, dir]);
    await annotator.annotate('mfr_path', treeRef('d000', 'song.flac'));
    // One call for the parent — the daemon walks the chain, not the client.
    expect(resolvePaths).toHaveBeenCalledTimes(1);
    expect(resolvePaths).toHaveBeenCalledWith('mfr_path', ['d000']);
  });

  test('the root contributes an empty path segment', async () => {
    const { annotator } = annotatorFor([root]);
    expect(await annotator.annotate('mfr_path', treeRef('r000', 'top.txt'))).toBe('top.txt');
  });

  test('a rootless tree_ref needs no annotation (the name is the path)', async () => {
    const { annotator, resolvePaths } = annotatorFor([]);
    expect(await annotator.annotate('genre', treeRef(null, 'jazz'))).toBeNull();
    expect(resolvePaths).not.toHaveBeenCalled();
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
