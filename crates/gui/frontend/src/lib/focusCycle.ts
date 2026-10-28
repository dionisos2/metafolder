// F6's walk (doc "Keyboard navigation"): `panel:focus cycle` goes from the
// left panel to the right one, then to the script's question when one awaits
// keys (doc "Script sessions"), then back to the first panel. Being "on the
// question" is having its keys taken: the keys it awaits go to the script.
//
// Pure: what to do, given where the focus is.

export type Slot = 'left' | 'right';

export interface CycleState {
  /** Both slots are shown. */
  split: boolean;
  focused: Slot;
  /** A script's question awaits keys in the workspace on screen. */
  question: boolean;
  /** The question has the keys (`script-keys:toggle`). */
  scriptKeys: boolean;
}

/** The next step: the slot to focus, and the script keys to set. */
export interface CycleStep {
  focus?: Slot;
  scriptKeys?: boolean;
}

export function cycleStep({ split, focused, question, scriptKeys }: CycleState): CycleStep {
  // On the question: back to the first panel, its keys given back.
  if (question && scriptKeys) return { scriptKeys: false, focus: split ? 'left' : focused };
  if (split && focused === 'left') return { focus: 'right' };
  if (question) return { scriptKeys: true };
  return split ? { focus: 'left' } : {};
}
