// The settings view is a modal dialog: while it is open it owns the keyboard
// (doc "Changing keybindings"). No binding fires behind it — tab walks its
// controls, enter presses the focused button, and a letter typed on a button
// cannot switch the workspace underneath — and escape closes it.

/** What the shell's key handler does with `key`: `close` the settings view,
 *  leave the key to the view (`own`), or null when the view is closed and the
 *  keybindings apply. */
export function settingsViewKey(open: boolean, key: string): 'close' | 'own' | null {
  if (!open) return null;
  return key === 'Escape' ? 'close' : 'own';
}
