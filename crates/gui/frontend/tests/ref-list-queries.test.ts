// ref-list panel query builder: metarecords whose chosen Ref field points to
// the selected tree node — exactly, or to the node or any of its descendants
// in the tree field's forest (classic tag inheritance).

import { describe, expect, test } from 'vitest';
import { refListQuery } from '../../default-config/panel-types/ref-list/queries.js';

describe('refListQuery', () => {
  test('exact: Ref points directly at the node', () => {
    expect(refListQuery({ refField: 'tag', treeField: 'tag_path', uuid: 'N', mode: 'exact' })).toEqual({
      type: 'follows',
      field: 'tag',
      target: { type: 'uuid_in', uuids: ['N'] },
    });
  });

  test('descendants: Ref points at the node OR any descendant in the tree field', () => {
    expect(
      refListQuery({ refField: 'tag', treeField: 'tag_path', uuid: 'N', mode: 'descendants' }),
    ).toEqual({
      type: 'follows',
      field: 'tag',
      target: {
        type: 'or',
        operands: [
          { type: 'uuid_in', uuids: ['N'] },
          { type: 'follows_transitive', field: 'tag_path', target: { type: 'uuid_in', uuids: ['N'] } },
        ],
      },
    });
  });

  test('unknown mode falls back to exact', () => {
    expect(refListQuery({ refField: 'tag', treeField: 'tag_path', uuid: 'N', mode: 'whatever' })).toEqual(
      refListQuery({ refField: 'tag', treeField: 'tag_path', uuid: 'N', mode: 'exact' }),
    );
  });
});
