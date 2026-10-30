// The key a context menu shows beside an entry (doc "Context menus").

import { lookupKeys } from '../../../panel-shim/keymatch.js';
import { bindingMatches, formatCombo } from '../../../panel-shim/keyhints.js';
import type { Binding } from './types';

/**
 * The key running `command` from a panel of type `panelType`, spelled for
 * reading: the shortest, and only one that would fire there, so a key that is
 * bound elsewhere, taken over by this panel, or whose command does not apply
 * here is not offered. A bare name takes any arguments, when no binding is the
 * exact invocation. Null when no key does.
 */
export function menuKey(
  bindings: Binding[],
  command: string,
  panelType: string | null,
  applies: (invocation: string, panelType: string | null) => boolean,
): string | null {
  const context = {
    panelType,
    textInput: false,
    focus: null,
    applies: (invocation: string) => applies(invocation, panelType),
  };
  const firing = bindings.filter(
    (binding) =>
      bindingMatches(binding, command) && lookupKeys(bindings, binding.keys, context).fired === binding,
  );
  const exact = firing.filter((binding) => binding.invocation === command);
  const [shortest] = (exact.length > 0 ? exact : firing).sort((a, b) => a.keys.length - b.keys.length);
  return shortest ? formatCombo(shortest.keys) : null;
}
