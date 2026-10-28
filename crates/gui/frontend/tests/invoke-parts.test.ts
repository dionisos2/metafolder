// `mf.invoke` takes an invocation as an array (doc "User commands"): the name,
// then each argument as one element — no quoting, no splitting — and
// `mf.ask` in place of an argument to ask for it, wherever it stands. It
// throws when the command fails or is abandoned, so a sequence stops there.

import { afterEach, describe, expect, test, vi } from 'vitest';
import {
  ask,
  type ArgPromptRequest,
  type ArgSpec,
  clearArgSpecs,
  clearUserCommands,
  collectArgs,
  CommandError,
  dispatch,
  installUserCommands,
} from '../src/lib/commands';
import { createUserCommandApi, type PanelApiDeps } from '../src/lib/panels/api';
import type { ExecContext } from '../src/lib/types';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async () => null),
  listen: vi.fn(async () => () => {}),
}));

afterEach(() => {
  clearUserCommands();
  clearArgSpecs();
});

const CONTEXT: ExecContext = { ws: 'ws-1', slot: 'left' };

function scriptedPrompt(answers: (string | null)[]) {
  const requests: ArgPromptRequest[] = [];
  let i = 0;
  const fn = vi.fn(async (request: ArgPromptRequest) => {
    requests.push(request);
    return answers[i++] ?? null;
  });
  return { fn, requests };
}

const spec = (name: string, initial?: string): ArgSpec => ({
  name,
  prompt: () => `${name}?`,
  ...(initial !== undefined ? { initial: () => initial } : {}),
});

describe('collectArgs with asked arguments', () => {
  test('an asked argument in the middle is asked, the others are taken as given', async () => {
    const { fn, requests } = scriptedPrompt(['rating']);
    const result = await collectArgs(
      [spec('scope'), spec('op'), spec('field'), spec('type'), spec('value')],
      ['selection', 'set', ask, 'int', '4'],
      fn,
    );
    expect(result).toEqual(['selection', 'set', 'rating', 'int', '4']);
    expect(requests.map((r) => r.argName)).toEqual(['field']);
  });

  test("ask({initial}) fills the line in place of the argument's own initial", async () => {
    const { fn, requests } = scriptedPrompt(['5']);
    await collectArgs([spec('field'), spec('value', 'spec-initial')], ['rating', ask({ initial: '4' })], fn);
    expect(requests[0].initial).toBe('4');
  });

  test('a bare ask keeps the argument its own initial', async () => {
    const { fn, requests } = scriptedPrompt(['5']);
    await collectArgs([spec('field'), spec('value', 'spec-initial')], ['rating', ask], fn);
    expect(requests[0].initial).toBe('spec-initial');
  });

  test('an optional argument named as asked is asked', async () => {
    const { fn, requests } = scriptedPrompt(['stay']);
    const result = await collectArgs([spec('zone'), { ...spec('stay'), optional: true }], ['finder', ask], fn);
    expect(result).toEqual(['finder', 'stay']);
    expect(requests.map((r) => r.argName)).toEqual(['stay']);
  });

  test('an element holding spaces is one argument, never split', async () => {
    const { fn } = scriptedPrompt([]);
    const result = await collectArgs([spec('zone'), spec('text'), spec('stay')], ['finder', 'two words', 'stay'], fn);
    expect(result).toEqual(['finder', 'two words', 'stay']);
  });

  test('the last argument still takes the elements left, joined', async () => {
    const { fn } = scriptedPrompt([]);
    const result = await collectArgs([spec('name')], ['My', 'music'], fn);
    expect(result).toEqual(['My music']);
  });

  test('an asked argument among the elements the last one takes is refused', async () => {
    const { fn } = scriptedPrompt([]);
    await expect(collectArgs([spec('name')], ['My', ask], fn)).rejects.toThrow(/ask/);
  });
});

describe('dispatch of an array', () => {
  test('runs the command with the elements as its arguments', async () => {
    const seen: unknown[][] = [];
    await installUserCommands(
      { 'user:x': { run: (_mf: unknown, ...args: unknown[]) => void seen.push(args) } },
      {},
      async () => {},
      () => {},
      () => ({}),
    );
    expect(await dispatch(['user:x', 'a b', 'c'], CONTEXT)).toEqual({ ok: true });
    expect(seen).toEqual([['a b', 'c']]);
  });

  test('an asked argument is asked through the command input', async () => {
    const seen: unknown[][] = [];
    await installUserCommands(
      {
        'user:x': {
          args: [{ name: 'a', prompt: () => 'a?' }, { name: 'b', prompt: () => 'b?' }],
          run: (_mf: unknown, ...args: unknown[]) => void seen.push(args),
        },
      },
      {},
      async () => {},
      () => {},
      () => ({}),
    );
    const done = dispatch(['user:x', ask, 'two'], CONTEXT);
    // The prompt is on screen: answer it as Enter would.
    const { store } = await import('../src/lib/store.svelte');
    await vi.waitFor(() => expect(store.ui.promptResolver).not.toBeNull());
    store.ui.promptResolver!('one');
    expect(await done).toEqual({ ok: true });
    expect(seen).toEqual([['one', 'two']]);
  });

  test('a command abandoned at a question reports it as cancelled', async () => {
    await installUserCommands(
      { 'user:y': { args: [{ name: 'a', prompt: () => 'a?' }], run: () => {} } },
      {},
      async () => {},
      () => {},
      () => ({}),
    );
    const done = dispatch(['user:y', ask], CONTEXT);
    const { store } = await import('../src/lib/store.svelte');
    await vi.waitFor(() => expect(store.ui.promptResolver).not.toBeNull());
    store.ui.promptResolver!(null);
    expect(await done).toEqual({ ok: true, cancelled: true });
  });
});

describe('mf.invoke', () => {
  function apiOver(result: unknown) {
    const dispatched: unknown[] = [];
    const deps: PanelApiDeps = {
      invoke: async () => null,
      dispatch: async (invocation: unknown) => {
        dispatched.push(invocation);
        return result;
      },
      registerHandler: () => {},
      registerArgs: () => {},
      onCommandsChanged: () => {},
      addDefaultMenuItems: () => {},
    };
    const mf = createUserCommandApi(deps, {
      guiServer: 'http://127.0.0.1:7524',
      sessionToken: 'token',
      context: () => CONTEXT,
    }) as unknown as {
      invoke: (parts: unknown) => Promise<void>;
      commands: { invoke: (parts: unknown) => Promise<void> };
      ask: unknown;
    };
    return { mf, dispatched };
  }

  test('hands the array to the dispatcher as it is', async () => {
    const { mf, dispatched } = apiOver({ ok: true });
    await mf.invoke(['panel:open', 'here', 'viewer']);
    await mf.commands.invoke(['message:clear']);
    expect(dispatched).toEqual([['panel:open', 'here', 'viewer'], ['message:clear']]);
  });

  test('offers ask', () => {
    const { mf } = apiOver({ ok: true });
    expect(mf.ask).toBe(ask);
  });

  test('refuses a string, naming the array form', async () => {
    const { mf, dispatched } = apiOver({ ok: true });
    await expect(mf.invoke('panel:open here viewer')).rejects.toThrow(/\['panel:open', 'here', 'viewer'\]/);
    expect(dispatched).toEqual([]);
  });

  test('throws a CommandError when the command fails', async () => {
    const { mf } = apiOver({ ok: false, error: 'no metarecord is selected' });
    const error = await mf.invoke(['metarecord:delete']).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(CommandError);
    expect((error as CommandError).message).toBe('no metarecord is selected');
    expect((error as CommandError).cancelled).toBe(false);
  });

  test('throws a cancelled CommandError when the command is abandoned', async () => {
    const { mf } = apiOver({ ok: true, cancelled: true });
    const error = await mf.invoke(['metarecord:delete']).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(CommandError);
    expect((error as CommandError).cancelled).toBe(true);
  });
});

describe('an uncaught CommandError', () => {
  test('is kept off the console: the status bar has said it', async () => {
    const { ignoreUncaughtCommandErrors } = await import('../src/lib/commands');
    const target = new EventTarget();
    const stop = ignoreUncaughtCommandErrors(target);
    const event = (reason: unknown) => {
      const e = new Event('unhandledrejection', { cancelable: true }) as Event & { reason: unknown };
      e.reason = reason;
      target.dispatchEvent(e);
      return e.defaultPrevented;
    };
    expect(event(new CommandError('no metarecord is selected'))).toBe(true);
    expect(event(new Error('a real bug'))).toBe(false);
    stop();
    expect(event(new CommandError('after'))).toBe(false);
  });
});
