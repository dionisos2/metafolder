// @ts-nocheck — not typed yet; the JS is being converted file by file.
// Progressive list loading (panel-shim/paged-list.js): the shared bits every
// long-list panel needs — the scroll-near-bottom threshold, the re-entrancy
// guard while a load is in flight, and the "loaded / total" footer text.
//
// It is deliberately headless and owns no DOM and no list state: `loaded` and
// `total` are read through panel-provided getters (a known array length, a
// daemon `count`, or `null` while unknown), and `loadMore` is opaque so the
// same controller fits both shapes we have:
//   - file / file-manager: the full list is known up front; loadMore enriches
//     (and renders) the next slice — the costly per-item daemon lookups are
//     what we bound, not the readDir;
//   - metarecord-list: the list itself is fetched page by page; loadMore pulls
//     the next page and hasMore tracks the cursor.
//
// The threshold/guard logic was duplicated near-identically across panels;
// this keeps it consistent (same 200px feel, same one-load-at-a-time rule).

export function createPagedList({
  loaded,
  total,
  hasMore = () => total() == null || loaded() < total(),
  loadMore,
  threshold = 200,
}) {
  let loading = false;

  /** Loads the next slice if not already loading, more remains, and the
   *  scroll position is within `threshold` px of the bottom. */
  async function maybeLoadMore(scrollEl) {
    if (loading || !hasMore()) return;
    if (scrollEl.scrollTop + scrollEl.clientHeight <= scrollEl.scrollHeight - threshold) return;
    loading = true;
    try {
      await loadMore();
    } finally {
      loading = false;
    }
  }

  /** Wires `maybeLoadMore` to the element's scroll event; returns a detach. */
  function attach(scrollEl) {
    const handler = () => void maybeLoadMore(scrollEl);
    scrollEl.addEventListener('scroll', handler);
    return () => scrollEl.removeEventListener('scroll', handler);
  }

  /** "200/5000", or just the count while the total is unknown — always
   *  shown, even once fully loaded ("5000/5000"). */
  function footerText() {
    const t = total();
    return t == null ? `${loaded()}` : `${loaded()}/${t}`;
  }

  return {
    attach,
    maybeLoadMore,
    footerText,
    get loading() {
      return loading;
    },
  };
}
