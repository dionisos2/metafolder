// The focused panel's Shadow DOM root, for shell features that act on whatever
// panel is on screen (the find bar). PanelHost owns the instance pool, so it
// installs the provider on mount — module-level, like `setPanelDispatch`, so a
// feature can reach the root without threading a prop through the component
// tree.

let provider: (() => Node | null) | null = null;

export function setFindRootProvider(fn: (() => Node | null) | null) {
  provider = fn;
}

/** The root of the panel in the focused slot, or null when there is none
 *  (no panel mounted yet, or no provider installed — as in the unit tests of
 *  everything but the panel host). */
export function focusedPanelRoot(): Node | null {
  return provider ? provider() : null;
}
