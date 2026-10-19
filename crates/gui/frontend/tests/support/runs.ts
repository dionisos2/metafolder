// A mounted panel's command handlers take first the API of the run (doc
// "Command transactions"), which the shell's dispatcher hands them. A test
// calling a captured handler directly stands for that dispatcher: `withRuns`
// wraps the stub API's `commands.register` so every handler it records is
// given the stub itself as that API — or `run`, a test telling the two apart.

import { vi } from 'vitest';

type Options = Record<string, unknown>;
type Register = (name: string, options?: Options) => unknown;
type Handler = (mf: unknown, ...args: unknown[]) => unknown;

/** `api`, its handlers registered bound to `run` (by default `api` itself);
 *  returned for `mount(root, withRuns(api))`. A `vi.fn()` register stays that
 *  mock, its calls what the panel passed. */
export function withRuns<T>(api: T, run: unknown = api): T {
  const commands = (api as { commands: { register: Register } }).commands;
  const original = commands.register;
  const record: Register = vi.isMockFunction(original)
    ? ((original.getMockImplementation() as Register | undefined) ?? (() => undefined))
    : original.bind(commands);
  const bound = (name: string, options: Options = {}) => {
    const handler = options.handler as Handler | undefined;
    return record(
      name,
      handler ? { ...options, handler: (...args: unknown[]) => handler(run, ...args) } : options,
    );
  };
  if (vi.isMockFunction(original)) original.mockImplementation(bound);
  else commands.register = bound;
  return api;
}
