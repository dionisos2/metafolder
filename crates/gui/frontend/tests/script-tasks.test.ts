import { describe, expect, it } from 'vitest';
import { ownedByVisible, scriptIndicator, scriptTasksState } from '../src/lib/store.svelte';

// The running-scripts loading indicator (spec-gui "Scripting"): the shell emits
// `script-task-changed` with the set of scripts still running; the task bar
// shows a spinner per entry (or a determinate bar once the script reports
// done/total via `mf gui progress`) so a slow script never looks frozen.

describe('scriptTasksState', () => {
  it('returns the running scripts from the payload', () => {
    expect(
      scriptTasksState({
        tasks: [{ task: 'script-1', workspace_id: 'ws-1', label: 'gui-tag-folder.sh' }],
      }),
    ).toEqual([{ task: 'script-1', workspace_id: 'ws-1', label: 'gui-tag-folder.sh' }]);
  });

  it('carries determinate progress when reported', () => {
    const tasks = scriptTasksState({
      tasks: [
        {
          task: 'script-1',
          workspace_id: 'ws-1',
          label: 'gui-tag-pair.sh',
          phase: '/music/x.mp3',
          done: 3,
          total: 10,
        },
      ],
    });
    expect(tasks[0].done).toBe(3);
    expect(tasks[0].total).toBe(10);
    expect(tasks[0].phase).toBe('/music/x.mp3');
  });

  it('is empty when nothing is running', () => {
    expect(scriptTasksState({ tasks: [] })).toEqual([]);
    expect(scriptTasksState({})).toEqual([]);
  });
});

// Scoping (spec-gui "Script session"): a script's question and task entry
// belong to the workspaces it owns — the launching one plus every workspace it
// created — so switching tab hides them instead of leaving them on screen.
describe('ownedByVisible', () => {
  it('is true when one owned workspace is on screen', () => {
    expect(ownedByVisible(['ws-1', 'ws-2'], ['ws-2'])).toBe(true);
    expect(ownedByVisible(['ws-1'], ['ws-1', 'ws-3'])).toBe(true);
  });

  it('is false when none is', () => {
    expect(ownedByVisible(['ws-1', 'ws-2'], ['ws-9'])).toBe(false);
    expect(ownedByVisible(['ws-1'], [])).toBe(false);
  });

  it('treats an unowned (empty/absent) list as always visible', () => {
    // A wait with no owner is not a script's: it must never be hidden.
    expect(ownedByVisible([], ['ws-1'])).toBe(true);
    expect(ownedByVisible(undefined, ['ws-1'])).toBe(true);
  });
});

describe('scriptTasksState waiting flag', () => {
  it('carries the waiting flag so the bar can stop spinning', () => {
    const tasks = scriptTasksState({
      tasks: [
        {
          task: 'script-1',
          workspace_id: 'ws-1',
          label: 'gui-tag-folder.sh',
          workspaces: ['ws-1', 'ws-2'],
          waiting: true,
        },
      ],
    });
    expect(tasks[0].waiting).toBe(true);
    expect(tasks[0].workspaces).toEqual(['ws-1', 'ws-2']);
  });

  it('defaults to not waiting', () => {
    const tasks = scriptTasksState({
      tasks: [{ task: 'script-1', workspace_id: 'ws-1', label: 'x.sh' }],
    });
    expect(tasks[0].waiting ?? false).toBe(false);
  });
});

// What the entry actually shows (spec-gui "Working vs. awaiting an answer").
// The two states are told apart by *motion*: reporting done/total used to
// replace the spinner with a determinate bar, and a bar that only advances
// when the user answers is exactly as still as a blocked one — so a working
// script looked idle. A working script spins, counts or no counts.
describe('scriptIndicator', () => {
  it('spins while the script works without counts', () => {
    expect(scriptIndicator({})).toEqual({
      awaiting: false,
      spinner: true,
      bar: false,
      counts: false,
    });
  });

  it('still spins while it works with counts, next to the determinate bar', () => {
    expect(scriptIndicator({ done: 3, total: 10 })).toEqual({
      awaiting: false,
      spinner: true,
      bar: true,
      counts: true,
    });
  });

  it('stops moving and says so while an answer is awaited', () => {
    expect(scriptIndicator({ waiting: true, done: 3, total: 10 })).toEqual({
      awaiting: true,
      spinner: false,
      bar: false,
      counts: true,
    });
  });

  it('awaits with no counts to show when the script reported none', () => {
    expect(scriptIndicator({ waiting: true })).toEqual({
      awaiting: true,
      spinner: false,
      bar: false,
      counts: false,
    });
  });

  it('needs both bounds before it shows a bar', () => {
    expect(scriptIndicator({ done: 3 }).bar).toBe(false);
    expect(scriptIndicator({ total: 10 }).bar).toBe(false);
    // A first tick at zero is a real count, not a missing one.
    expect(scriptIndicator({ done: 0, total: 10 }).bar).toBe(true);
  });
});
