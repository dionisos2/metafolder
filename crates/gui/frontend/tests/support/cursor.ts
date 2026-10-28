// Drives a mounted panel's cursor the way the shell does (doc "Generic panel
// commands"): the panel's stub `metafolder.provide` is a `vi.fn()`, and the
// generic command runs on what the panel handed it — each function given first
// the API of the run, as the shell does (doc "Command transactions"). Here
// that is the stub API the panel provided it through: the mock's `this`.

import type { FileOps } from '../../src/lib/fileCommands';
import {
  bindFunctions,
  type PanelFunctions,
  type ProvidedFunctions,
  runGeneric,
} from '../../src/lib/panelFunctions';

type ProvideMock = { mock: { calls: unknown[][]; contexts: unknown[] } };

/** What `stubFileOps` reads of a stub API (whichever parts it has). */
type StubApi = {
  global?: { get?(k: string): unknown; set(k: string, v: unknown): unknown };
  fs?: Partial<Record<'readDir' | 'move' | 'copy' | 'mkdir' | 'createFile', (...a: string[]) => unknown>>;
  trash?: { trashPath?(repo: string | null, path: string): unknown };
  statusBar?: { message?(text: string, ms?: number): unknown };
  workspace?: { set?(k: string, v: unknown): unknown };
};

/** The `file:` commands' operations (doc "File commands") over a stub API:
 *  its `fs`, its trash, `window.prompt` for the questions. */
export function stubFileOps(api: StubApi): FileOps {
  const fs = api.fs ?? {};
  return {
    readDir: async (dir) => ((await fs.readDir?.(dir)) ?? []) as { name: string }[],
    move: async (from, to) => void (await fs.move?.(from, to)),
    copy: async (from, to) => void (await fs.copy?.(from, to)),
    mkdir: async (path) => void (await fs.mkdir?.(path)),
    createFile: async (path) => void (await fs.createFile?.(path)),
    trash: async (path) => void (await api.trash?.trashPath?.(null, path)),
    record: async () => true,
    ask: async (question, initial) => window.prompt(question, initial),
    clipboard: async () =>
      ((await api.global?.get?.('clipboard')) ?? null) as Awaited<ReturnType<FileOps['clipboard']>>,
    clearClipboard: async () => void (await api.global?.set('clipboard', null)),
    status: async (text) => void (await api.statusBar?.message?.(text)),
    changed: async () => void (await api.workspace?.set?.('metarecords:dirty', Date.now())),
  };
}

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
  // The clipboard commands write the global variable, through the stub's
  // `global` when it has one.
  const api = provide.mock.contexts[0] as StubApi | undefined;
  await runGeneric(name, provided(provide), args, {
    panelType: 'panel',
    reinvoke: (invocation) => reinvoked.push(invocation),
    clip: async (paths, mode) => void (await api?.global?.set('clipboard', { paths, mode })),
    files: api ? stubFileOps(api) : undefined,
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
