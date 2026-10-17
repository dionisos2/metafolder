// The shared list cursor (panel-shim/list-cursor.js): what every panel listing
// items does when its cursor moves. A held arrow key repeats faster than a
// panel can rebuild its rows or publish the selection (two workspace.set IPC
// round-trips), and anything slower than the repeat rate piles key events up
// that keep replaying after the key is released. So moving the cursor only
// retargets a class on the rows already there, and the publication is
// coalesced: one in flight, one trailing with the final position.

import { describe, expect, test, vi } from 'vitest';
import { createListCursor } from '../../panel-shim/list-cursor.js';

function list(n: number, tag = 'li'): HTMLElement {
  const parent = document.createElement(tag === 'tr' ? 'tbody' : 'ul');
  for (let i = 0; i < n; i += 1) {
    const row = document.createElement(tag);
    row.textContent = `row ${i}`;
    parent.append(row);
  }
  return parent;
}

/** An async gate the test opens by hand. */
function gate() {
  let open: () => void = () => {};
  const promise = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { promise, open };
}

describe('createListCursor', () => {
  test('moving retargets the class onto the row at the index, rows untouched', async () => {
    const rows = list(3);
    const before = [...rows.children];
    const cursor = createListCursor({ containers: () => [rows] });
    await cursor.move(1);
    expect([...rows.children].map((r) => r.classList.contains('cursor'))).toEqual([false, true, false]);
    await cursor.move(2);
    expect([...rows.children].map((r) => r.classList.contains('cursor'))).toEqual([false, false, true]);
    expect([...rows.children]).toEqual(before);
  });

  test('the class name is configurable', async () => {
    const rows = list(2, 'tr');
    const cursor = createListCursor({ containers: () => [rows], className: 'selected' });
    await cursor.move(0);
    expect(rows.children[0].classList.contains('selected')).toBe(true);
    expect(rows.children[0].classList.contains('cursor')).toBe(false);
  });

  test('every container is kept in step (a table and a grid of the same rows)', async () => {
    const table = list(3, 'tr');
    const grid = list(3);
    const cursor = createListCursor({ containers: () => [table, grid] });
    await cursor.move(2);
    expect(table.children[2].classList.contains('cursor')).toBe(true);
    expect(grid.children[2].classList.contains('cursor')).toBe(true);
  });

  test('a stray class left by a render is cleared too', async () => {
    const rows = list(3);
    rows.children[0].classList.add('cursor');
    const cursor = createListCursor({ containers: () => [rows] });
    await cursor.move(2);
    expect(rows.querySelectorAll('.cursor')).toHaveLength(1);
  });

  test('an index past the rows only clears the highlight', async () => {
    const rows = list(2);
    const cursor = createListCursor({ containers: () => [rows] });
    await cursor.move(0);
    await cursor.move(5);
    expect(rows.querySelectorAll('.cursor')).toHaveLength(0);
  });

  test('the highlighted row is scrolled into view', async () => {
    const rows = list(3);
    const scroll = vi.fn();
    (rows.children[1] as HTMLElement).scrollIntoView = scroll;
    const cursor = createListCursor({ containers: () => [rows] });
    await cursor.move(1);
    expect(scroll).toHaveBeenCalledWith({ block: 'nearest' });
  });

  test('a burst of moves publishes once in flight, then once with the final index', async () => {
    const rows = list(10);
    const first = gate();
    const published: number[] = [];
    const cursor = createListCursor({
      containers: () => [rows],
      publish: async (index: number) => {
        published.push(index);
        if (index === 0) await first.promise;
      },
    });
    const moves = [0, 1, 2, 3].map((i) => cursor.move(i));
    // The highlight follows every move at once, whatever the publication.
    expect(rows.children[3].classList.contains('cursor')).toBe(true);
    expect(published).toEqual([0]);
    first.open();
    await Promise.all(moves);
    expect(published).toEqual([0, 3]);
  });

  test('publish alone re-publishes without touching the highlight, in the same coalescing', async () => {
    const rows = list(3);
    const published: number[] = [];
    const cursor = createListCursor({
      containers: () => [rows],
      publish: async (index: number) => void published.push(index),
    });
    await cursor.move(1);
    rows.children[1].classList.remove('cursor');
    await cursor.publish(1);
    expect(published).toEqual([1, 1]);
    expect(rows.querySelectorAll('.cursor')).toHaveLength(0);
  });

  test('without publish, move resolves at once', async () => {
    const rows = list(1);
    const cursor = createListCursor({ containers: () => [rows] });
    await expect(cursor.move(0)).resolves.toBeUndefined();
  });
});
