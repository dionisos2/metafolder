// A panel instance belongs to one workspace (PanelHost.svelte): the ids
// `ws-N` are reused, so a workspace closed and another opened under its
// number in between two frames — or while the window is minimised, where no
// frame comes — must not hand the new one the instance of the old.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import PanelHost from '../src/lib/panels/PanelHost.svelte';
import { store } from '../src/lib/store.svelte';
import type { WorkspaceInfo } from '../src/lib/types';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (command: string) => (command === 'global_vars' ? {} : null)),
  listen: vi.fn(async () => () => {}),
}));

const frames: FrameRequestCallback[] = [];

function workspace(id: string): WorkspaceInfo {
  return { id, name: id, active_repo: null, repo_name: null };
}

describe('a workspace number reused', () => {
  let component: ReturnType<typeof mount> | null = null;

  afterEach(() => {
    if (component) void unmount(component);
    component = null;
    document.body.replaceChildren();
    vi.unstubAllGlobals();
  });

  test('gets panel instances of its own', async () => {
    // No frame comes unless the test runs it: a minimised window.
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => frames.push(cb));
    vi.stubGlobal('fetch', vi.fn(async () => new Response('<html><body><p>x</p></body></html>')));
    const runFrames = () => {
      for (const cb of frames.splice(0)) cb(0);
    };

    store.panelTypes = [];
    store.workspaces = [workspace('ws-1')];
    store.layout = {
      left: { visible: true, workspace_id: 'ws-1', panel_type: 'message' },
      right: { visible: false, workspace_id: null, panel_type: null },
      focused: 'left',
    };
    const target = document.createElement('div');
    document.body.append(target);
    component = mount(PanelHost, { target });
    flushSync();
    runFrames();
    const before = document.querySelector('.panel-host');
    expect(before).not.toBeNull();

    // Closed, then another workspace opened under its number, no frame between.
    store.workspaces = [];
    store.layout = {
      left: { visible: true, workspace_id: null, panel_type: null },
      right: { visible: false, workspace_id: null, panel_type: null },
      focused: 'left',
    };
    flushSync();
    await Promise.resolve();
    store.workspaces = [workspace('ws-1')];
    store.layout = {
      left: { visible: true, workspace_id: 'ws-1', panel_type: 'message' },
      right: { visible: false, workspace_id: null, panel_type: null },
      focused: 'left',
    };
    flushSync();
    runFrames();

    const hosts = document.querySelectorAll('.panel-host');
    expect(hosts).toHaveLength(1);
    expect(hosts[0]).not.toBe(before);
  });
});
