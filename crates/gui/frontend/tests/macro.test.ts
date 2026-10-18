// Macros (doc "Macros"): `macro:toggle-recording` records the commands run
// from the keyboard — keys and the command input — with the arguments they
// ran with; `macro:ask`, in a question, makes that argument asked again at
// each run; `macro:run` replays the recording in order and stops at the first
// command that did not run; `macro:save <name>` writes it to macros.js as the
// user command `user:<name>`.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';

const { tauri } = vi.hoisted(() => ({ tauri: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

import {
  ask,
  clearArgSpecs,
  clearUserCommands,
  dispatch,
  installUserCommands,
} from '../src/lib/commands';
import { macroEntry, macros, resetMacros } from '../src/lib/macro';
import { store } from '../src/lib/store.svelte';
import type { ExecContext } from '../src/lib/types';

const CONTEXT: ExecContext = { ws: 'ws-1', slot: 'left' };
const KEY = { record: true } as const;

let ran: string[][];

beforeEach(async () => {
  ran = [];
  tauri.length = 0;
  resetMacros();
  store.commands = [];
  await installUserCommands(
    {
      'user:a': { run: (_mf: unknown, ...args: string[]) => void ran.push(['user:a', ...args]) },
      'user:two': {
        args: [
          { name: 'x', prompt: () => 'x?' },
          { name: 'y', prompt: () => 'y?' },
        ],
        run: (_mf: unknown, ...args: string[]) => void ran.push(['user:two', ...args]),
      },
      'user:fail': {
        run: () => {
          throw new Error('nope');
        },
      },
      'user:quiet': { log: false, run: () => void ran.push(['user:quiet']) },
    },
    {},
    async () => {},
    () => {},
    () => ({}),
  );
  store.commands = ['user:a', 'user:two', 'user:fail'].map((name) => ({
    name,
    label: name,
    owner: null,
    log: true,
  })) as never;
  store.commands.push({ name: 'user:quiet', label: 'q', owner: null, log: false } as never);
});

afterEach(() => {
  clearUserCommands();
  clearArgSpecs();
});

/** Answers the prompt on screen, once there is one, as the command input
 *  does: release the input, then hand the value (null: Escape) over. */
async function answer(value: string | null) {
  await vi.waitFor(() => expect(store.ui.promptResolver).not.toBeNull());
  const resolve = store.ui.promptResolver!;
  store.ui.promptText = null;
  store.ui.promptResolver = null;
  resolve(value);
}

describe('recording', () => {
  test('records the commands run from the keyboard, with their arguments', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.recording()).toBe(true);
    await dispatch('user:a one', CONTEXT, KEY);
    await dispatch(['user:a', 'two words'], CONTEXT, KEY);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.recording()).toBe(false);
    expect(macros.last()).toEqual([['user:a', 'one'], ['user:a', 'two words']]);
  });

  test('records the answers given to the questions', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    const done = dispatch('user:two', CONTEXT, KEY);
    await answer('1');
    await answer('2');
    await done;
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.last()).toEqual([['user:two', '1', '2']]);
  });

  test('macro:ask in a question makes that argument asked at each run', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    const done = dispatch('user:two', CONTEXT, KEY);
    await vi.waitFor(() => expect(store.ui.promptResolver).not.toBeNull());
    await dispatch('macro:ask', CONTEXT, KEY);
    await answer('1');
    await answer('2');
    await done;
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.last()).toEqual([['user:two', ask, '2']]);
  });

  test('macro:ask twice in the same question takes it back', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    const done = dispatch('user:two', CONTEXT, KEY);
    await vi.waitFor(() => expect(store.ui.promptResolver).not.toBeNull());
    await dispatch('macro:ask', CONTEXT, KEY);
    await dispatch('macro:ask', CONTEXT, KEY);
    await answer('1');
    await answer('2');
    await done;
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.last()).toEqual([['user:two', '1', '2']]);
  });

  test('macro:ask with no question to mark says so', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(await dispatch('macro:ask', CONTEXT, KEY)).toEqual({
      ok: false,
      error: 'macro:ask marks the question on screen, while recording',
    });
  });

  test('what does not come from the keyboard is not recorded', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    // A script's `mf gui command`, a panel's button, a user command's mf.invoke.
    await dispatch('user:a script', CONTEXT);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.last()).toBeNull();
  });

  test('a failed command, an abandoned one and a log: false one are not recorded', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    await dispatch('user:fail', CONTEXT, KEY);
    const done = dispatch('user:two', CONTEXT, KEY);
    await answer(null);
    await done;
    await dispatch('user:quiet', CONTEXT, KEY);
    await dispatch('user:a kept', CONTEXT, KEY);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.last()).toEqual([['user:a', 'kept']]);
  });

  test('a shell line is recorded as typed', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    await dispatch('!echo hi', CONTEXT, KEY);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.last()).toEqual([['!echo hi']]);
  });

  test('an empty recording keeps the macro recorded before', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    await dispatch('user:a one', CONTEXT, KEY);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(macros.last()).toEqual([['user:a', 'one']]);
  });

  test('the status bar shows the recording', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(store.ui.macroRecording).toBe(true);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(store.ui.macroRecording).toBe(false);
  });
});

describe('macro:run', () => {
  async function record(...invocations: string[]) {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    for (const invocation of invocations) await dispatch(invocation, CONTEXT, KEY);
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    ran = [];
  }

  test('runs the recorded commands in order', async () => {
    await record('user:a one', 'user:two 1 2');
    expect(await dispatch('macro:run', CONTEXT, KEY)).toEqual({ ok: true });
    expect(ran).toEqual([['user:a', 'one'], ['user:two', '1', '2']]);
  });

  test('asks again what was marked', async () => {
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    const recorded = dispatch('user:two', CONTEXT, KEY);
    await vi.waitFor(() => expect(store.ui.promptResolver).not.toBeNull());
    await dispatch('macro:ask', CONTEXT, KEY);
    await answer('1');
    await answer('2');
    await recorded;
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    ran = [];
    const done = dispatch('macro:run', CONTEXT, KEY);
    await answer('other');
    expect(await done).toEqual({ ok: true });
    expect(ran).toEqual([['user:two', 'other', '2']]);
  });

  test('stops at the first command that fails, saying which', async () => {
    await record('user:a one');
    macros.setLast([['user:a', 'one'], ['user:fail'], ['user:a', 'never']]);
    const result = await dispatch('macro:run', CONTEXT, KEY);
    expect(result).toEqual({ ok: false, error: 'macro stopped at step 2 of 3 (user:fail): nope' });
    expect(ran).toEqual([['user:a', 'one']]);
  });

  test('is refused while recording, and with nothing recorded', async () => {
    expect(await dispatch('macro:run', CONTEXT, KEY)).toEqual({ ok: false, error: 'no macro recorded yet' });
    await dispatch('macro:toggle-recording', CONTEXT, KEY);
    expect(await dispatch('macro:run', CONTEXT, KEY)).toEqual({
      ok: false,
      error: 'a macro is being recorded: stop it first (macro:toggle-recording)',
    });
  });
});

describe('macro:save', () => {
  test('writes the last macro to macros.js under user:<name>', async () => {
    macros.setLast([['user:a', 'one'], ['user:two', ask, "it's"]]);
    expect(await dispatch('macro:save rate-all', CONTEXT, KEY)).toEqual({ ok: true });
    const call = tauri.find((c) => c.cmd === 'macro_save');
    expect(call?.args).toEqual({ name: 'user:rate-all', entry: macroEntry('user:rate-all', macros.last()!) });
  });

  test('a name with its user: prefix is taken as it is', async () => {
    macros.setLast([['user:a', 'one']]);
    await dispatch('macro:save user:x', CONTEXT, KEY);
    expect(tauri.find((c) => c.cmd === 'macro_save')?.args).toMatchObject({ name: 'user:x' });
  });

  test('a name a command already has is refused', async () => {
    macros.setLast([['user:a', 'one']]);
    expect(await dispatch('macro:save a', CONTEXT, KEY)).toEqual({
      ok: false,
      error: 'user:a is already a command: pick another name',
    });
  });

  test('a name that is not one word is refused', async () => {
    macros.setLast([['user:a', 'one']]);
    const result = await dispatch(['macro:save', 'two words'], CONTEXT, KEY);
    expect(result.ok).toBe(false);
  });

  test('nothing recorded, nothing saved', async () => {
    expect(await dispatch('macro:save x', CONTEXT, KEY)).toEqual({ ok: false, error: 'no macro recorded yet' });
  });
});

describe('macroEntry', () => {
  test('is the user command one would write by hand', () => {
    expect(
      macroEntry('user:x', [
        ['panel:open', 'here', 'file'],
        ['metarecord:bulk', 'selection', 'set', ask, 'int', '4'],
        ['!echo %'],
        ['workspace:rename', "it's \\ here"],
      ]),
    ).toBe(
      [
        "  'user:x': {",
        "    label: 'Macro user:x',",
        '    run: async (mf) => {',
        "      await mf.invoke(['panel:open', 'here', 'file']);",
        "      await mf.invoke(['metarecord:bulk', 'selection', 'set', mf.ask, 'int', '4']);",
        "      await mf.invoke(['!echo %']);",
        "      await mf.invoke(['workspace:rename', 'it\\'s \\\\ here']);",
        '    },',
        '  },',
        '',
      ].join('\n'),
    );
  });

  test('a value holding a line break stays one line of code', () => {
    expect(macroEntry('user:x', [['user:a', 'a\nb']])).toContain("['user:a', 'a\\nb']");
  });
});
