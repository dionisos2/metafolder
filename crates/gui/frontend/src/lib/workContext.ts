// The GUI's work context, as the slot header shows it (doc "The GUI's work
// context").

import type { WorkContext } from './types';

/** What the slot header's context indicator shows for `context`: the name, and
 *  on hover the label, the description and how to close it — null when no
 *  context is in force. */
export function contextIndicator(
  context: WorkContext | null | undefined,
): { text: string; title: string } | null {
  if (!context) return null;
  const lines = [`work context "${context.name}" (label ${context.label})`];
  if (context.description) lines.push(context.description);
  lines.push('every change is written in it — context:unset closes it');
  return { text: context.name, title: lines.join('\n') };
}
