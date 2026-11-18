// The task bar and the question bar take their own place in the layout: shown
// from their first entry on, kept (empty) once their entries are gone, hidden
// and shown again by the user, and shown again by every new entry (doc "Status
// bar and task bar").

import { describe, expect, test, vi } from 'vitest';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async () => null),
  listen: vi.fn(async () => () => {}),
}));
import { BAR_NAMES, createBars } from '../src/lib/bars';

describe('bars', () => {
  test('a bar is not shown before its first entry', () => {
    const bars = createBars();
    expect(bars.shown('tasks')).toBe(false);
    expect(bars.shown('question')).toBe(false);
  });

  test('a first entry shows the bar', () => {
    const bars = createBars();
    bars.note('tasks', ['a']);
    expect(bars.shown('tasks')).toBe(true);
    expect(bars.shown('question')).toBe(false);
  });

  test('the bar stays once its entries are gone', () => {
    const bars = createBars();
    bars.note('tasks', ['a']);
    bars.note('tasks', []);
    expect(bars.shown('tasks')).toBe(true);
  });

  test('toggling hides a shown bar and shows a hidden one', () => {
    const bars = createBars();
    bars.note('question', ['1']);
    bars.toggle('question');
    expect(bars.shown('question')).toBe(false);
    bars.toggle('question');
    expect(bars.shown('question')).toBe(true);
  });

  test('toggling a bar that never showed shows it, empty', () => {
    const bars = createBars();
    bars.toggle('tasks');
    expect(bars.shown('tasks')).toBe(true);
  });

  test('a hidden bar stays hidden while its entries only change', () => {
    const bars = createBars();
    bars.note('tasks', ['a', 'b']);
    bars.toggle('tasks');
    bars.note('tasks', ['a', 'b']);
    bars.note('tasks', ['b']);
    bars.note('tasks', []);
    expect(bars.shown('tasks')).toBe(false);
  });

  test('a new entry shows a hidden bar again', () => {
    const bars = createBars();
    bars.note('tasks', ['a']);
    bars.toggle('tasks');
    bars.note('tasks', ['a', 'b']);
    expect(bars.shown('tasks')).toBe(true);
  });

  test('an entry that comes back after leaving is new', () => {
    const bars = createBars();
    bars.note('tasks', ['call']);
    bars.note('tasks', []);
    bars.toggle('tasks');
    bars.note('tasks', ['call']);
    expect(bars.shown('tasks')).toBe(true);
  });

  test('the bars are independent', () => {
    const bars = createBars();
    bars.note('tasks', ['a']);
    bars.note('question', ['1']);
    bars.toggle('tasks');
    bars.note('question', ['2']);
    expect(bars.shown('tasks')).toBe(false);
    expect(bars.shown('question')).toBe(true);
  });

  test('listeners hear every change of what is shown, and only those', () => {
    const bars = createBars();
    const heard: Record<string, boolean>[] = [];
    bars.subscribe((state) => heard.push({ ...state }));
    bars.note('tasks', ['a']);
    bars.note('tasks', ['a', 'b']);
    bars.toggle('tasks');
    expect(heard).toEqual([
      { tasks: true, question: false },
      { tasks: false, question: false },
    ]);
  });

  test('the names are the arguments of bar:toggle', () => {
    expect([...BAR_NAMES]).toEqual(['tasks', 'question']);
  });
});

describe('bar:toggle', () => {
  test('toggles the bar its argument names', async () => {
    const { builtins } = await import('../src/lib/builtins');
    const { bars } = await import('../src/lib/bars');
    const toggle = builtins()['bar:toggle'];
    expect(toggle).toBeDefined();
    const before = bars.shown('tasks');
    await toggle(['tasks'], { ws: null, slot: 'left' } as never, 'bar:toggle');
    expect(bars.shown('tasks')).toBe(!before);
    expect(bars.shown('question')).toBe(false);
  });

  test('refuses a name that is no bar', async () => {
    const { builtins } = await import('../src/lib/builtins');
    const toggle = builtins()['bar:toggle'];
    await expect(
      (async () => toggle(['nope'], { ws: null, slot: 'left' } as never, 'bar:toggle'))(),
    ).rejects.toThrow(/tasks.*question/);
  });
});
