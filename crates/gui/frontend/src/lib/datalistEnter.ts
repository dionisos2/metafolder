// Enter in a field completing over a <datalist> (doc "Keybindings"): with a
// suggestion highlighted in the browser's drop-down, the key is the browser's,
// to fill the field with it. Nothing tells the page whether the drop-down is
// open, nor which suggestion is highlighted, and WebKitGTK fills the field only
// after the key's event is over, so the field cannot be watched for the pick.
// What the page does see is the arrows: a suggestion is highlighted once Up or
// Down was pressed in the field, until another key or the field is left.

/** Whether `element` is a text field completing over a datalist that offers
 *  something. The list is looked up in the field's own tree: a panel's ids
 *  live in its shadow root. */
export function completesOverDatalist(element: Element | null): element is HTMLInputElement {
  if (!(element instanceof HTMLInputElement)) return false;
  const id = element.getAttribute('list');
  if (!id) return false;
  const root = element.getRootNode() as Document | ShadowRoot;
  const list = root.getElementById?.(id);
  return list instanceof HTMLDataListElement && list.options.length > 0;
}

/** The fields whose drop-down has a suggestion highlighted. */
const highlighted = new WeakSet<HTMLInputElement>();

/** Follows what key `combo` does to the drop-down of the field `target`, and
 *  says whether the key is the browser's: Enter with a suggestion highlighted,
 *  which picks it. Any other Enter is left to the keybindings — it validates
 *  the field and leaves it. */
export function datalistTakesKey(target: Element | null, combo: string): boolean {
  if (!completesOverDatalist(target)) return false;
  if (combo === 'down' || combo === 'up') {
    if (!highlighted.has(target)) {
      highlighted.add(target);
      target.addEventListener('blur', () => highlighted.delete(target), { once: true });
    }
    return false;
  }
  const picks = combo === 'enter' && highlighted.has(target);
  // Typing filters the suggestions anew, Escape closes them, Enter has picked.
  highlighted.delete(target);
  return picks;
}
