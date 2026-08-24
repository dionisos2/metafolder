import { describe, expect, it } from 'vitest';
import { scriptTasksState } from '../src/lib/store.svelte';

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
