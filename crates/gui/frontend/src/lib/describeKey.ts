// `help:key` — the Emacs `C-h k` (spec-gui "Help"): the shell swallows the next
// key sequence and reports the command it runs instead of running it. Pure:
// keys.ts feeds one combo at a time and applies the returned step to the store.
// The lookup runs in the context the key arrived in (focused panel type, focus
// scope, typing or not) — so the answer is what the key *would do now* — and a
// combo bound only in other contexts says so instead of reading as unbound.

import { lookupKeys } from '../../../panel-shim/keymatch.js';
import { formatCombo } from '../../../panel-shim/keyhints.js';
import type { Binding } from './types';

/** The context one key is described in — the matcher's, as the key arrived. */
export interface DescribeContext {
  panelType?: string | null;
  textInput?: boolean;
  focus?: string | null;
}

/** One `help:key` step — a union, not a bag of optional fields: `keys` grows
 *  only while a sequence is pending, `message` exists exactly when the wait is
 *  over and the report is due. */
export type DescribeStep =
  | { pending: true; keys: string[]; candidates: Binding[] }
  | { reported: true; message: string }
  | { cancelled: true };

/**
 * What one more combo does during a `help:key` wait: escape cancels, a combo
 * that only starts a sequence waits for the rest (its continuations in
 * `candidates`, as the pending hint shows), anything else ends the wait with
 * the report.
 */
export function describeKeyStep(
  described: string[],
  combo: string,
  table: Binding[],
  context: DescribeContext,
  labelOf: (name: string) => string,
): DescribeStep {
  if (combo === 'escape') return { cancelled: true };
  const keys = [...described, combo];
  // The shim's JSDoc Binding is the looser shape (scope fields optional); the
  // compiled table the Rust engine emits fills them all in.
  const found = lookupKeys(table, keys, context) as {
    fired: Binding | null;
    pending: Binding[];
    elsewhere: Binding[];
  };
  if (!found.fired && found.pending.length > 0) {
    return { pending: true, keys, candidates: found.pending };
  }
  return { reported: true, message: describeMessage(keys, found, context, labelOf) };
}

/** The report line for a finished sequence: the invocation the key runs and
 *  the command's description, or the fact that it runs nothing — naming the
 *  scopes it *is* bound in when the exclusion is contextual, so a panel-local
 *  shortcut does not read as unbound from another panel. */
function describeMessage(
  keys: string[],
  found: { fired: Binding | null; elsewhere: Binding[] },
  context: DescribeContext,
  labelOf: (name: string) => string,
): string {
  const combo = formatCombo(keys);
  if (found.fired) {
    // Only the first token is a name: a binding may pre-fill arguments
    // (`panel:set type metarecord-list`), and a shell invocation (`!…`) names
    // no command at all. The description is the registry label.
    const name = found.fired.invocation.trim().split(/\s+/)[0] ?? '';
    const label = name.startsWith('!') ? '' : labelOf(name);
    return label
      ? `${combo} runs ${found.fired.invocation} — ${label}`
      : `${combo} runs ${found.fired.invocation}`;
  }
  const entries = [
    ...new Set(
      found.elsewhere.map((binding) => `${binding.invocation} ${scopeGap(binding, context)}`),
    ),
  ];
  return entries.length === 0
    ? `${combo} is not bound to any command`
    : `${combo} is not bound to any command here (elsewhere: ${entries.join(', ')})`;
}

/** Where a binding fires, as far as the current context is from it: only the
 *  dimensions that exclude it are named ("in file-manager", "in the finder
 *  widget of metarecord-list", "when not typing") — a gap phrase always names
 *  at least one, since the binding is in the elsewhere list. */
function scopeGap(binding: Binding, context: DescribeContext): string {
  if (binding.focus != null) {
    const widget = `in the ${binding.focus} widget`;
    return binding.when != null && binding.when !== (context.panelType ?? null)
      ? `${widget} of ${binding.when}`
      : widget;
  }
  const parts: string[] = [];
  if (binding.when != null && binding.when !== (context.panelType ?? null)) {
    parts.push(`in ${binding.when}`);
  }
  if (!binding.text_input && context.textInput) parts.push('when not typing');
  return parts.join(' ');
}
