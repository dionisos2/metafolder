// Drives a mounted panel's cursor the way the shell does (doc "Generic panel
// commands"): the panel's stub `metafolder.provide` is a `vi.fn()`, and the
// generic command runs on what the panel handed it — each function given first
// the API of the run, as the shell does (doc "Command transactions"). Here
// that is the stub API the panel provided it through: the mock's `this`.

import {
  bindFunctions,
  type PanelFunctions,
  type ProvidedFunctions,
  runGeneric,
} from '../../src/lib/panelFunctions';

type ProvideMock = { mock: { calls: unknown[][]; contexts: unknown[] } };

/** Everything the panel provided, later calls overriding earlier ones, bound
 *  to the API it was provided through. */
export function provided(provide: ProvideMock): PanelFunctions {
  const bound = provide.mock.calls.map((call, i) =>
    bindFunctions(call[0] as ProvidedFunctions, provide.mock.contexts[i] as Metafolder.Api),
  );
  return Object.assign({}, ...bound) as PanelFunctions;
}

/**
 * Runs the generic command `name` (`view:refresh`, `cursor:find`…) on the
 * panel, `args` being its arguments after the panel. What a walk asks again is
 * returned.
 */
export async function generic(
  provide: ProvideMock,
  name: string,
  ...args: string[]
): Promise<string[]> {
  const reinvoked: string[] = [];
  await runGeneric(name, provided(provide), args, {
    panelType: 'panel',
    reinvoke: (invocation) => reinvoked.push(invocation),
  });
  return reinvoked;
}

/** `cursor:<verb> <args…>`, e.g. `cursor(provide, 'next')`,
 *  `cursor(provide, 'find', 'walk', 'sub/')`. */
export function cursor(provide: ProvideMock, verb: string, ...args: string[]): Promise<string[]> {
  return generic(provide, `cursor:${verb}`, ...args);
}

/** The candidates `cursor:find` offers for the panel. */
export async function findCandidates(provide: ProvideMock): Promise<string[]> {
  const rows = (await provided(provide).entries?.()) ?? [];
  return rows.map((row) => row.label ?? row.name);
}
