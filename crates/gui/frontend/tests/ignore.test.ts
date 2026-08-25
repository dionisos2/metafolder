// The ignore half of the GUI (spec-gui "Ignore patterns"): the panel API
// surface, the target/copy-on-write resolution shared by every write, and the
// four `ignore:*` commands.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { createPanelApi } from '../src/lib/panels/api';
import {
  ignorePresetCandidates,
  resolvePresetName,
  ignoreTarget,
  patternForPath,
  patternForExtension,
  __resetIgnoreCaches,
  targetDir,
} from '../src/lib/ignore';

function setup() {
  const invoke = vi.fn(async (_cmd: string, _args?: unknown) => ({ status: 200, body: null }) as unknown);
  const instance = createPanelApi(
    {
      invoke,
      dispatch: vi.fn(async () => {}),
      registerHandler: vi.fn(),
      onCommandsChanged: vi.fn(),
      addDefaultMenuItems: vi.fn(),
    },
    {
      wsId: 'ws-1',
      panelType: 'file-manager',
      guiServer: 'http://127.0.0.1:7524',
      sessionToken: 'test-token',
      root: {} as ShadowRoot,
      visibilityGate: { visible: true, set() {}, whenVisible: vi.fn() },
    },
  );
  return { api: instance.api as any, invoke };
}

describe('panel api — ignore', () => {
  test('each ignore method maps to its Tauri command with the right args', async () => {
    const { api, invoke } = setup();

    await api.ignore.presets();
    expect(invoke).toHaveBeenCalledWith('ignore_presets');

    await api.ignore.current('repo1', 'uuid1');
    expect(invoke).toHaveBeenCalledWith('ignore_current', { repo: 'repo1', target: 'uuid1' });

    await api.ignore.apply('repo1', 'uuid1', ['git'], 'add');
    expect(invoke).toHaveBeenCalledWith('ignore_apply', {
      repo: 'repo1',
      target: 'uuid1',
      presets: ['git'],
      mode: 'add',
    });

    await api.ignore.write('repo1', 'uuid1', ['a', 'b']);
    expect(invoke).toHaveBeenCalledWith('ignore_write', {
      repo: 'repo1',
      target: 'uuid1',
      patterns: ['a', 'b'],
    });
  });
});

describe('ad-hoc pattern construction', () => {
  test('a path pattern is anchored and regex-escaped', () => {
    // `/work/a+b.txt` inside a scope rooted at `/work` matches `/a+b.txt`
    // exactly, plus anything below it when it is a directory.
    expect(patternForPath('/a+b.txt')).toBe('^/a\\+b\\.txt(/.*)?$');
  });

  test('an extension pattern matches that extension in the given directory', () => {
    expect(patternForExtension('/photos', 'jpg')).toBe('^/photos/[^/]+\\.jpg$');
    expect(patternForExtension('', 'jpg')).toBe('^/[^/]+\\.jpg$');
  });
});

describe('preset completion', () => {
  beforeEach(() => __resetIgnoreCaches());

  test('candidates are "<name> — <description>" lines, resolvable back', async () => {
    const invoke = vi.fn(async (cmd: string) => {
      expect(cmd).toBe('ignore_presets');
      return [
        { name: 'git', description: 'Git metadata', patterns: ['a'] },
        { name: 'node', description: '', patterns: ['b'] },
      ];
    });
    const lines = await ignorePresetCandidates(invoke as any);
    expect(lines).toEqual(['git — Git metadata', 'node']);
    expect(await resolvePresetName('git — Git metadata', invoke as any)).toBe('git');
    expect(await resolvePresetName('node', invoke as any)).toBe('node');
    expect(await resolvePresetName('nope', invoke as any)).toBe(null);
  });
});

describe('target resolution and copy-on-write', () => {
  beforeEach(() => __resetIgnoreCaches());

  /** A daemon stub: `effective` drives GET /ignore/effective, and every
   *  tree/resolve-path answers with `uuid`. */
  function daemonStub(effective: Record<string, unknown>, uuid: string | null = 'dir-uuid') {
    return vi.fn(async (method: string, path: string, _body?: unknown) => {
      if (path.includes('/ignore/effective')) return effective;
      if (path.endsWith('/tree/resolve-path')) return { uuid };
      throw new Error(`unexpected call ${method} ${path}`);
    });
  }

  test('a directory with its own set is written without a prompt', async () => {
    const call = daemonStub({ source: '/work', direct: true, patterns: ['x'] });
    const confirm = vi.fn(() => true);
    const target = await ignoreTarget(
      { call: call as any, repo: 'repo1', relPath: '/work', confirm },
      );
    expect(target).toEqual({ uuid: 'dir-uuid', relPath: '/work', copied: [] });
    expect(confirm).not.toHaveBeenCalled();
  });

  test('an inherited set is copied onto the target first, after confirmation', async () => {
    const call = daemonStub({ source: '', direct: false, patterns: ['p1', 'p2'] });
    const confirm = vi.fn((_question: string) => true);
    const target = await ignoreTarget({
      call: call as any,
      repo: 'repo1',
      relPath: '/work/live',
      confirm,
    });
    expect(confirm).toHaveBeenCalledTimes(1);
    const question = confirm.mock.calls[0][0] as unknown as string;
    expect(question).toContain('2');
    expect(question).toContain('/work/live');
    expect(target).toEqual({ uuid: 'dir-uuid', relPath: '/work/live', copied: ['p1', 'p2'] });
  });

  test('declining the copy leaves the target empty rather than cancelling', async () => {
    const call = daemonStub({ source: '', direct: false, patterns: ['p1'] });
    const target = await ignoreTarget({
      call: call as any,
      repo: 'repo1',
      relPath: '/work/live',
      confirm: () => false,
    });
    expect(target).toEqual({ uuid: 'dir-uuid', relPath: '/work/live', copied: [] });
  });

  test('nothing inherited means no prompt', async () => {
    const call = daemonStub({ source: null, direct: false, patterns: [] });
    const confirm = vi.fn(() => true);
    const target = await ignoreTarget({ call: call as any, repo: 'repo1', relPath: '/x', confirm });
    expect(confirm).not.toHaveBeenCalled();
    expect(target?.copied).toEqual([]);
  });

  test('an unresolvable directory yields no target', async () => {
    const call = daemonStub({ source: null, direct: false, patterns: [] }, null);
    const target = await ignoreTarget({
      call: call as any,
      repo: 'repo1',
      relPath: '/gone',
      confirm: () => true,
    });
    expect(target).toBe(null);
  });
});

describe('the ignore commands', () => {
  test('add/remove/set declare a completing preset argument at module load', async () => {
    const { argSpecFor } = await import('../src/lib/commands');
    for (const name of ['ignore:add', 'ignore:remove', 'ignore:set']) {
      expect(argSpecFor(name)?.map((s) => s.name), name).toEqual(['preset']);
    }
    expect(argSpecFor('ignore:list'), 'list takes no argument').toBeUndefined();
  });
});

describe('the command target directory', () => {
  const call = (paths: Record<string, string[]>, type: string) =>
    vi.fn(async (method: string, path: string) => {
      if (path.endsWith('/resolve-tree')) {
        const uuid = path.split('/metarecords/')[1].split('/')[0];
        return { paths: paths[uuid] ?? [] };
      }
      if (path.includes('/metarecords/')) {
        return { fields: [{ name: 'mfr_type', value: { type: 'string', value: type } }] };
      }
      throw new Error(`unexpected ${method} ${path}`);
    });

  test('the file manager’s directory wins, as a repo-relative path', async () => {
    const rel = await targetDir({
      call: call({}, 'dir') as any,
      repo: 'r',
      repoRoot: '/home/u/music',
      fmDir: '/home/u/music/live/2024',
      selected: null,
    });
    expect(rel).toBe('/live/2024');
  });

  test('the repository root itself is the empty path', async () => {
    const rel = await targetDir({
      call: call({}, 'dir') as any,
      repo: 'r',
      repoRoot: '/home/u/music',
      fmDir: '/home/u/music',
      selected: null,
    });
    expect(rel).toBe('');
  });

  test('a directory outside the repo falls back to the selection, then the root', async () => {
    const rel = await targetDir({
      call: call({}, 'dir') as any,
      repo: 'r',
      repoRoot: '/home/u/music',
      fmDir: '/etc',
      selected: null,
    });
    expect(rel).toBe('');
  });

  test('a selected directory targets itself', async () => {
    const rel = await targetDir({
      call: call({ u1: ['/live'] }, 'dir') as any,
      repo: 'r',
      repoRoot: '/home/u/music',
      fmDir: null,
      selected: { uuid: 'u1' },
    });
    expect(rel).toBe('/live');
  });

  test('a selected file targets its containing directory', async () => {
    const rel = await targetDir({
      call: call({ u1: ['/live/set.flac'] }, 'file') as any,
      repo: 'r',
      repoRoot: '/home/u/music',
      fmDir: null,
      selected: { uuid: 'u1' },
    });
    expect(rel).toBe('/live');
  });

  test('a selected file at the top level targets the repository root', async () => {
    const rel = await targetDir({
      call: call({ u1: ['/set.flac'] }, 'file') as any,
      repo: 'r',
      repoRoot: '/home/u/music',
      fmDir: null,
      selected: { uuid: 'u1' },
    });
    expect(rel).toBe('');
  });
});
