// Opening a loaded repository (`repos:switch`), as the shipped `commands.js`
// defines it since the command left the shell builtins: the pick completes
// over the daemon's loaded repositories, and the choice opens exactly like a
// click in the repos panel — adopted in the focused workspace when it has no
// repository yet, otherwise a new workspace (a workspace's `active_repo`
// cannot change).
//
// The command is configuration now, so what is pinned is the resolution and
// the two ways out: reshaping the wording is free; opening the wrong
// repository — or in the wrong workspace — is not.

import { beforeEach, describe, expect, test } from 'vitest';
import shipped from '../../default-config/commands.js';

const REPOS = [
  { repo_uuid: 'u-1', name: 'Music', root: '/srv/music' },
  { repo_uuid: 'a-b-c-d', name: 'Films', root: '/srv/films' },
];

const calls = {
  daemon: [] as string[],
  adopted: [] as string[],
  invoked: [] as string[],
};

const state = {
  activeRepo: null as unknown,
};

/** The `mf` a user command is handed (doc "User commands"), faked. */
function fakeMf() {
  return {
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? state.activeRepo : null),
      set: async () => {},
      adoptRepo: async (repo: string) => {
        calls.adopted.push(repo);
      },
    },
    daemon: {
      call: async (_method: string, path: string) => {
        calls.daemon.push(path);
        return REPOS;
      },
    },
    invoke: (invocation: string) => {
      calls.invoked.push(invocation);
      return Promise.resolve({ ok: true });
    },
    statusBar: {
      message: async () => {},
      error: async (error: unknown) => {
        void error;
      },
    },
  };
}

/** The completion pass, exactly as the command input runs it (it rebuilds the
 *  picked-line map on the way). */
function complete() {
  return shipped['repos:switch'].args[0].complete(fakeMf() as never);
}

function run(choice: string) {
  return shipped['repos:switch'].run(fakeMf() as never, choice);
}

beforeEach(() => {
  for (const list of Object.values(calls)) list.length = 0;
  state.activeRepo = null;
});

describe('the repo pick', () => {
  test('the loaded repositories are offered as "<name> — <root>" lines', async () => {
    expect(await complete()).toEqual(['Music — /srv/music', 'Films — /srv/films']);
    expect(calls.daemon).toEqual(['/repos']);
  });

  test('the picked line names its repository', async () => {
    const lines = await complete();

    await run(lines[1]);

    expect(calls.adopted).toEqual(['a-b-c-d']);
  });
});

describe('opening the pick', () => {
  test('a workspace without a repository adopts it, where the repos panel would', async () => {
    await run('u-1');

    expect(calls.adopted).toEqual(['u-1']);
    // And shows it in the list, panel replaced in place.
    expect(calls.invoked).toEqual(['panel:open here metarecord-list']);
  });

  test('a workspace with a repository opens a new one on the pick', async () => {
    state.activeRepo = 'u-9';

    await run('u-1');

    // `active_repo` cannot change: a different repository takes a workspace.
    expect(calls.adopted).toEqual([]);
    expect(calls.invoked).toEqual(['workspace:new u-1']);
  });

  test('a bare repo name resolves', async () => {
    state.activeRepo = 'u-9';

    await run('Films');

    expect(calls.invoked).toEqual(['workspace:new a-b-c-d']);
  });

  test('a uuid resolves with or without its dashes', async () => {
    state.activeRepo = 'u-9';

    await run('abcd');
    expect(calls.invoked).toEqual(['workspace:new a-b-c-d']);

    calls.invoked.length = 0;
    await run('a-b-c-d');
    expect(calls.invoked).toEqual(['workspace:new a-b-c-d']);
  });

  test('nothing matches: the failure is thrown, for the shell to report', async () => {
    // `dispatch` posts a handler's failure to the status bar itself (and the
    // "Error: " prefix it carries is its doing, not this entry's).
    await expect(run('Nope')).rejects.toThrow('no loaded repository matches "Nope"');
    expect(calls.adopted).toEqual([]);
    expect(calls.invoked).toEqual([]);
  });
});
