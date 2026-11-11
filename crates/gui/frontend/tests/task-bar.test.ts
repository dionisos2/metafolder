// The task bar's grace period. The bar is a row of the shell's column layout,
// so it appearing takes height away from the panels — every panel re-lays out,
// mid-scroll, mid-read. A watcher flush lasts milliseconds and happens whenever
// anything at all touches a watched file, so without a grace the screen twitches
// for work nobody asked to see (doc "Following a task").

import { describe, expect, test } from 'vitest';
import { settledTasks, TASK_GRACE_MS, tasksOfRepos } from '../src/lib/working';

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

// A daemon task matters where its repository is in use: only there do actions
// queue behind it. A workspace on another repository works normally and has no
// use for it (doc "Status bar and task bar").
describe('tasksOfRepos', () => {
  const t = (id: string, repo_uuid: string) => ({ id, repo_uuid });

  test('keeps only the tasks of the given repositories', () => {
    const tasks = [t('a', 'r1'), t('b', 'r2'), t('c', 'r1')];
    expect(tasksOfRepos(tasks, ['r1']).map((x) => x.id)).toEqual(['a', 'c']);
  });

  test('no repository on screen shows no task', () => {
    expect(tasksOfRepos([t('a', 'r1')], [])).toEqual([]);
    expect(tasksOfRepos([t('a', 'r1')], [null])).toEqual([]);
  });

  test('a dashed uuid matches the same one in hex form', () => {
    const dashed = '0123abcd-0000-4000-8000-00000000beef';
    const hex = dashed.replace(/-/g, '');
    expect(tasksOfRepos([t('a', hex)], [dashed]).map((x) => x.id)).toEqual(['a']);
    expect(tasksOfRepos([t('a', dashed)], [hex]).map((x) => x.id)).toEqual(['a']);
  });

  test('uuids compare regardless of case', () => {
    expect(tasksOfRepos([t('a', 'ABCD')], ['abcd']).map((x) => x.id)).toEqual(['a']);
  });
});
