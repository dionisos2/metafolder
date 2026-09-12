// The task bar's grace period. The bar is a row of the shell's column layout,
// so it appearing takes height away from the panels — every panel re-lays out,
// mid-scroll, mid-read. A watcher flush lasts milliseconds and happens whenever
// anything at all touches a watched file, so without a grace the screen twitches
// for work nobody asked to see (spec-tasks "GUI").

import { describe, expect, test } from 'vitest';
import { settledTasks, TASK_GRACE_MS } from '../src/lib/working';

function task(id: string) {
  return { id, kind: 'flush' };
}

describe('settledTasks', () => {
  test('a task seen for the first time shows nothing', () => {
    const seen = new Map<string, number>();
    expect(settledTasks(seen, [task('a')], 1000)).toEqual([]);
  });

  test('a task that vanishes before the grace never shows', () => {
    const seen = new Map<string, number>();
    settledTasks(seen, [task('a')], 1000);
    settledTasks(seen, [task('a')], 1000 + TASK_GRACE_MS - 1);
    // Gone: the flush finished between two polls, as the ordinary one does.
    expect(settledTasks(seen, [], 1000 + TASK_GRACE_MS)).toEqual([]);
    // And it is forgotten, so a later task reusing nothing of it starts fresh.
    expect(seen.size).toBe(0);
  });

  test('a task still there after the grace is shown', () => {
    const seen = new Map<string, number>();
    settledTasks(seen, [task('a')], 1000);
    const shown = settledTasks(seen, [task('a')], 1000 + TASK_GRACE_MS);
    expect(shown.map((t) => t.id)).toEqual(['a']);
  });

  test('each task is timed from its own first sighting', () => {
    const seen = new Map<string, number>();
    settledTasks(seen, [task('old')], 0);
    const shown = settledTasks(seen, [task('old'), task('new')], TASK_GRACE_MS);
    expect(shown.map((t) => t.id)).toEqual(['old']);
  });

  test('a task that comes back after vanishing is timed afresh', () => {
    const seen = new Map<string, number>();
    settledTasks(seen, [task('a')], 0);
    settledTasks(seen, [], 10);
    settledTasks(seen, [task('a')], 20);
    expect(settledTasks(seen, [task('a')], 20 + TASK_GRACE_MS - 1)).toEqual([]);
    expect(settledTasks(seen, [task('a')], 20 + TASK_GRACE_MS).map((t) => t.id)).toEqual(['a']);
  });

  test('the order the daemon listed the tasks in is kept', () => {
    const seen = new Map<string, number>();
    settledTasks(seen, [task('a'), task('b')], 0);
    expect(settledTasks(seen, [task('a'), task('b')], TASK_GRACE_MS).map((t) => t.id)).toEqual([
      'a',
      'b',
    ]);
  });
});
