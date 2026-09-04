// The shell's find-bar driver (spec-gui "Find in panel"): it re-runs the
// search against the focused panel's root on every change, so a panel that
// re-rendered under it can never leave a stale match behind, and it wraps
// around at both ends.

import { beforeEach, describe, expect, test } from 'vitest';
import { closeFind, currentRange, runFind, stepFind, stepIndex } from '../src/lib/find';
import { setFindRootProvider } from '../src/lib/panels/roots';
import { store } from '../src/lib/store.svelte';

let root: HTMLElement;

beforeEach(() => {
  document.body.innerHTML = '<div id="panel"><p>alpha</p><p>beta alpha gamma</p></div>';
  root = document.getElementById('panel')!;
  setFindRootProvider(() => root);
  closeFind();
});

describe('stepIndex', () => {
  test('wraps forward past the last match', () => {
    expect(stepIndex(2, 1, 3)).toBe(0);
  });

  test('wraps backward past the first match', () => {
    expect(stepIndex(0, -1, 3)).toBe(2);
  });

  test('has no index without a match', () => {
    expect(stepIndex(0, 1, 0)).toBe(-1);
  });

  test('starts at the first match when there was no current one', () => {
    expect(stepIndex(-1, 1, 3)).toBe(0);
    expect(stepIndex(-1, -1, 3)).toBe(2);
  });
});

describe('runFind', () => {
  test('counts the matches and selects the first one', () => {
    runFind('alpha');
    expect(store.ui.find.count).toBe(2);
    expect(store.ui.find.index).toBe(0);
    expect(currentRange()?.toString()).toBe('alpha');
  });

  test('reports no match rather than failing', () => {
    runFind('nothing here');
    expect(store.ui.find.count).toBe(0);
    expect(store.ui.find.index).toBe(-1);
    expect(currentRange()).toBe(null);
  });

  test('an emptied needle clears the matches', () => {
    runFind('alpha');
    runFind('');
    expect(store.ui.find.count).toBe(0);
    expect(store.ui.find.index).toBe(-1);
  });

  test('keeps the current match position when the needle is re-run', () => {
    runFind('alpha');
    stepFind(1);
    expect(store.ui.find.index).toBe(1);
    runFind('alpha');
    expect(store.ui.find.index).toBe(1);
  });

  test('re-runs against the panel as it is now, not as it was', () => {
    runFind('alpha');
    expect(store.ui.find.count).toBe(2);
    root.innerHTML = '<p>alpha</p>';
    runFind('alpha');
    expect(store.ui.find.count).toBe(1);
    expect(store.ui.find.index).toBe(0);
  });
});

describe('stepFind', () => {
  test('walks the matches and wraps around', () => {
    runFind('alpha');
    stepFind(1);
    expect(store.ui.find.index).toBe(1);
    stepFind(1);
    expect(store.ui.find.index).toBe(0);
    stepFind(-1);
    expect(store.ui.find.index).toBe(1);
  });

  test('is a no-op without matches', () => {
    runFind('zzz');
    stepFind(1);
    expect(store.ui.find.index).toBe(-1);
  });
});

describe('closeFind', () => {
  test('closes the bar and drops the matches', () => {
    runFind('alpha');
    closeFind();
    expect(store.ui.find.open).toBe(false);
    expect(store.ui.find.count).toBe(0);
    expect(currentRange()).toBe(null);
  });
});
