// The history:* commands (doc "Input history"): the field their argument
// designates, and what they do to it. The walk itself is the field's own
// (panel-shim/history.js).

import { historyOf } from '../../../panel-shim/history.js';

export const SELF_FOCUS = 'self-focus';
export const HISTORY_COMMANDS = ['history:prev', 'history:next', 'history:search'];

type Root = ParentNode | null;

function namedFields(root: Root): HTMLElement[] {
  return [...(root?.querySelectorAll<HTMLElement>('[data-mf-focus]') ?? [])];
}

/** The field `arg` designates, when it has a history: the focused one for
 *  `self-focus`, else the field of that name in the focused panel. */
export function resolveHistoryField(
  arg: string,
  active: Element | null,
  root: Root,
): HTMLElement | null {
  const field =
    arg === SELF_FOCUS
      ? (active as HTMLElement | null)
      : (namedFields(root).find((el) => el.dataset.mfFocus === arg) ?? null);
  return historyOf(field) ? field : null;
}

/** The names of the panel's fields that have a history. */
export function historyFieldNames(root: Root): string[] {
  return namedFields(root)
    .filter((el) => historyOf(el))
    .map((el) => el.dataset.mfFocus as string);
}

/** Whether a key bound to `invocation` fires here: a history command only on a
 *  field having a history, so that its key passes through elsewhere. Any other
 *  command always applies. */
export function historyApplies(invocation: string, active: Element | null, root: Root): boolean {
  const [name, field] = invocation.trim().split(/\s+/);
  if (!HISTORY_COMMANDS.includes(name)) return true;
  return resolveHistoryField(field ?? '', active, root) !== null;
}

/** Runs a history command. A field named rather than focused takes the focus:
 *  what is recalled lands where the user goes on typing. */
export async function runHistoryCommand(
  name: string,
  arg: string | undefined,
  active: Element | null,
  root: Root,
): Promise<void> {
  if (!arg) throw new Error(`which field? (${SELF_FOCUS}, or a field of the focused panel)`);
  const field = resolveHistoryField(arg, active, root);
  const history = historyOf(field);
  if (!field || !history) {
    throw new Error(
      arg === SELF_FOCUS
        ? 'no text field with a history has the focus'
        : `no field "${arg}" with a history in the focused panel`,
    );
  }
  if (arg !== SELF_FOCUS) field.focus();
  if (name === 'history:prev') await history.prev();
  else if (name === 'history:next') history.next();
  else await history.search();
}
