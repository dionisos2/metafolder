// Progressive list loading (panel-shim/paged-list.js): the shared scroll
// threshold + re-entrancy guard + footer formatting reused by the file,
// file-manager and metarecord-list panels. `loaded`/`total` are owned by
// the panel and read through getters; `loadMore` is opaque (fetch a page
// OR enrich the next in-memory slice), so the same controller fits both
// the daemon-paginated and the known-list-with-lazy-enrichment cases.

import { describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel shim
import { createPagedList } from '../../panel-shim/paged-list.js';

// A scroll container is read for three numbers only; tests pass plain
// objects so the threshold logic is exercised without a layout engine.
const at = (scrollTop: number, clientHeight: number, scrollHeight: number) => ({
  scrollTop,
  clientHeight,
  scrollHeight,
});

describe('createPagedList', () => {
  test('loads more when scrolled within the threshold of the bottom', async () => {
    const loadMore = vi.fn(async () => {});
    const pager = createPagedList({ loaded: () => 0, total: () => 100, loadMore });
    await pager.maybeLoadMore(at(1000, 0, 1000)); // 1000 > 1000 - 200
    expect(loadMore).toHaveBeenCalledTimes(1);
  });

  test('does not load when far from the bottom', async () => {
    const loadMore = vi.fn(async () => {});
    const pager = createPagedList({ loaded: () => 0, total: () => 100, loadMore });
    await pager.maybeLoadMore(at(0, 200, 1000)); // 200 <= 1000 - 200
    expect(loadMore).not.toHaveBeenCalled();
  });

  test('respects a custom threshold', async () => {
    const loadMore = vi.fn(async () => {});
    const pager = createPagedList({ loaded: () => 0, total: () => 100, loadMore, threshold: 50 });
    await pager.maybeLoadMore(at(800, 100, 1000)); // 900 <= 950 → no load at 50px
    expect(loadMore).not.toHaveBeenCalled();
    await pager.maybeLoadMore(at(860, 100, 1000)); // 960 > 950 → load
    expect(loadMore).toHaveBeenCalledTimes(1);
  });

  test('guards against re-entrant loads while one is pending', async () => {
    let release: () => void = () => {};
    const loadMore = vi.fn(() => new Promise<void>((r) => (release = r)));
    const pager = createPagedList({ loaded: () => 0, total: () => 100, loadMore });

    const first = pager.maybeLoadMore(at(1000, 0, 1000));
    expect(pager.loading).toBe(true);
    void pager.maybeLoadMore(at(1000, 0, 1000)); // ignored while pending
    expect(loadMore).toHaveBeenCalledTimes(1);

    release();
    await first;
    expect(pager.loading).toBe(false);
    const second = pager.maybeLoadMore(at(1000, 0, 1000)); // free again
    expect(loadMore).toHaveBeenCalledTimes(2);
    release();
    await second;
  });

  test('does not load once exhausted (default hasMore = loaded < total)', async () => {
    const loadMore = vi.fn(async () => {});
    const pager = createPagedList({ loaded: () => 100, total: () => 100, loadMore });
    await pager.maybeLoadMore(at(1000, 0, 1000));
    expect(loadMore).not.toHaveBeenCalled();
  });

  test('default hasMore is true while total is unknown', async () => {
    const loadMore = vi.fn(async () => {});
    const pager = createPagedList({ loaded: () => 50, total: () => null, loadMore });
    await pager.maybeLoadMore(at(1000, 0, 1000));
    expect(loadMore).toHaveBeenCalledTimes(1);
  });

  test('a custom hasMore overrides the default (cursor-style pagination)', async () => {
    const loadMore = vi.fn(async () => {});
    let cursor: string | null = 'next';
    const pager = createPagedList({
      loaded: () => 50,
      total: () => 100,
      hasMore: () => cursor !== null,
      loadMore,
    });
    cursor = null;
    await pager.maybeLoadMore(at(1000, 0, 1000));
    expect(loadMore).not.toHaveBeenCalled();
  });

  test('footerText always shows the count, even fully loaded', () => {
    let loaded = 0;
    let total: number | null = null;
    const pager = createPagedList({ loaded: () => loaded, total: () => total, loadMore: async () => {} });
    expect(pager.footerText()).toBe('0'); // unknown total
    total = 5000;
    loaded = 200;
    expect(pager.footerText()).toBe('200/5000');
    loaded = 5000;
    expect(pager.footerText()).toBe('5000/5000'); // still shown when complete
  });

  test('attach wires a scroll listener and returns a detach', () => {
    const handlers: Record<string, () => void> = {};
    const scrollEl = {
      ...at(1000, 0, 1000),
      addEventListener: (name: string, fn: () => void) => (handlers[name] = fn),
      removeEventListener: vi.fn(),
    };
    const loadMore = vi.fn(async () => {});
    const pager = createPagedList({ loaded: () => 0, total: () => 100, loadMore });

    const detach = pager.attach(scrollEl);
    expect(handlers.scroll).toBeTypeOf('function');
    handlers.scroll();
    expect(loadMore).toHaveBeenCalledTimes(1);

    detach();
    expect(scrollEl.removeEventListener).toHaveBeenCalledWith('scroll', handlers.scroll);
  });
});
