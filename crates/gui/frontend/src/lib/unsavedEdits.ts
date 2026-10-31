// Edits in progress (doc "Edits in progress"): text a panel holds that nothing has
// saved — a field of the detail panel, a file in the viewer. A panel says so through
// `metafolder.provide({ unsaved, discard })`, and the shell asks before what would
// lose it: a move of the selection, closing a workspace, quitting.

import { functionProvider } from './panelFunctions';

const SELECTION = new Set(['selected_metarecord', 'selected_paths']);

// A writer of the selection writes the two variables one after the other: refusing
// the first refuses the second, without a second question. Held until the writer's
// next write has run, which is before the next task.
let refusedWorkspace: string | null = null;

/** Whether the selection of workspace `ws` may move, having asked if it would
 *  leave an edit. Agreeing discards the edit. For what moves along with the
 *  selection, a list's cursor, to ask before it moves. */
export function mayMoveSelection(ws: string): boolean {
  const edits = functionProvider()?.unsaved(ws) ?? [];
  if (edits.length === 0) return true;
  if (!confirm(`Unsaved changes in ${names(edits)} — discard them and move the selection?`)) {
    return false;
  }
  for (const edit of edits) edit.discard();
  return true;
}

/** Whether the write of `key` may go on, having asked if it would leave an edit
 *  of workspace `ws`. Agreeing discards the edit. `read` gives the variable's value
 *  now: writing the same selection again is no move. */
export async function confirmSelectionWrite(
  ws: string,
  key: string,
  value: unknown,
  read: (key: string) => Promise<unknown>,
): Promise<boolean> {
  if (!SELECTION.has(key)) return true;
  if (refusedWorkspace === ws) return false;
  const edits = functionProvider()?.unsaved(ws) ?? [];
  if (edits.length === 0) return true;
  if (JSON.stringify((await read(key)) ?? null) === JSON.stringify(value ?? null)) return true;
  // The edit may have ended, or the move been refused, during the read.
  if (refusedWorkspace === ws) return false;
  if (mayMoveSelection(ws)) return true;
  refusedWorkspace = ws;
  setTimeout(() => {
    refusedWorkspace = null;
  }, 0);
  return false;
}

/** Whether closing workspace `ws`, or the whole GUI when null, may go on. */
export function confirmClosing(ws: string | null): boolean {
  const edits = functionProvider()?.unsaved(ws) ?? [];
  if (edits.length === 0) return true;
  return confirm(`Unsaved changes in ${names(edits)} — discard them and close?`);
}

function names(edits: { describe: string }[]): string {
  return edits.map((edit) => edit.describe).join(', ');
}
