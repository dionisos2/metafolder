// @ts-nocheck — not typed yet; the JS is being converted file by file.
// Query builder for the ref-list panel. Pure function, no daemon access —
// unit-tested in frontend/tests/ref-list-queries.test.js.

// The Query IR for the metarecords whose `refField` (a Ref field) points to the
// selected tree node `uuid`:
//   - mode 'exact'       : the Ref referent is the node itself.
//   - mode 'descendants' : the Ref referent is the node OR any descendant of it
//     in `treeField`'s forest (classic tag inheritance — selecting "music" also
//     surfaces things tagged "music/rock"). FollowsTransitive excludes its own
//     roots, so the node itself is OR-ed back in.
export function refListQuery({ refField, treeField, uuid, mode }) {
  const self = { type: 'uuid_in', uuids: [uuid] };
  const target =
    mode === 'descendants'
      ? {
          type: 'or',
          operands: [
            self,
            { type: 'follows_transitive', field: treeField, target: self },
          ],
        }
      : self;
  return { type: 'follows', field: refField, target };
}
