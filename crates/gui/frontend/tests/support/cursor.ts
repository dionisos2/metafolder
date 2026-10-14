// Drives a mounted panel's cursor the way the shell does (doc "Generic panel
// commands"): the panel's stub `metafolder.provide` is a `vi.fn()`, and the
// generic command runs on what the panel handed it.

import { type PanelFunctions, runGeneric } from '../../src/lib/panelFunctions';

type ProvideMock = { mock: { calls: unknown[][] } };

/** Everything the panel provided, later calls overriding earlier ones. */
export function provided(provide: ProvideMock): PanelFunctions {
  return Object.assign({}, ...provide.mock.calls.map((call) => call[0] as PanelFunctions));
}

/**
 * Runs `cursor:<verb> <args…>` on the panel, e.g. `cursor(provide, 'next')`,
 * `cursor(provide, 'find', 'walk', 'sub/')`. What a walk asks again is pushed
 * onto `reinvoked`.
 */
export async function cursor(
  provide: ProvideMock,
  verb: string,
  ...args: string[]
): Promise<string[]> {
  const reinvoked: string[] = [];
  await runGeneric(`cursor:${verb}`, provided(provide), args, {
    panelType: 'panel',
    reinvoke: (invocation) => reinvoked.push(invocation),
  });
  return reinvoked;
}

/** The candidates `cursor:find` offers for the panel. */
export async function findCandidates(provide: ProvideMock): Promise<string[]> {
  const rows = (await provided(provide).entries?.()) ?? [];
  return rows.map((row) => row.label ?? row.name);
}
