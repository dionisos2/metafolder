import { describe, expect, it } from 'vitest';
import { scriptTasksState } from '../src/lib/store.svelte';

// The running-scripts loading indicator (spec-gui "Scripting"): the shell emits
// `script-task-changed` with the set of scripts still running; the task bar
// shows a spinner per entry so a slow script never looks frozen.

describe('scriptTasksState', () => {
  it('returns the running scripts from the payload', () => {
    expect(
      scriptTasksState({ tasks: [{ workspace_id: 'ws-1', label: 'gui-tag-folder.sh' }] }),
    ).toEqual([{ workspace_id: 'ws-1', label: 'gui-tag-folder.sh' }]);
  });

  it('is empty when nothing is running', () => {
    expect(scriptTasksState({ tasks: [] })).toEqual([]);
    expect(scriptTasksState({})).toEqual([]);
  });
});
