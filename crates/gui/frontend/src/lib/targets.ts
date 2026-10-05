// Open targets (doc "Panel slots and layout"): where a command that opens a
// panel type does its work — `here`, in place of its context's panel; `other`,
// the same workspace in the other slot; `new`, a fork of the workspace. Shared
// by the `panel:open` builtin and `mf.atTarget`, so the two cannot disagree.

import type { ExecContext } from './types';

export const TARGETS = ['here', 'other', 'new'] as const;
export type Target = (typeof TARGETS)[number];

export function isTarget(value: unknown): value is Target {
  return (TARGETS as readonly unknown[]).includes(value);
}

/** Refuses `where` unless it is one of `allowed` (every target by default). */
export function checkTarget(where: unknown, allowed: readonly string[] = TARGETS): Target {
  if (!isTarget(where) || !allowed.includes(where)) {
    throw new Error(`not a target here: "${String(where)}" (expected ${allowed.join(' / ')})`);
  }
  return where;
}

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

/** Moves to `where` from `context` and answers the context there: `other`
 *  shows the workspace in the other slot, `new` forks it (the fork opens where
 *  a new tab does, in the context's slot). */
export async function resolveTarget(
  where: Target,
  context: ExecContext,
  invoke: Invoke,
): Promise<ExecContext> {
  if (where === 'here') return context;
  if (!context.ws) throw new Error('no workspace to open from');
  if (where === 'other') {
    const slot = context.slot === 'left' ? 'right' : 'left';
    await invoke('tab_assign', { wsId: context.ws, slot });
    return { ws: context.ws, slot };
  }
  const fork = (await invoke('workspace_fork', { wsId: context.ws })) as string;
  return { ws: fork, slot: context.slot };
}
