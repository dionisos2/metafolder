// The settings view owns the keyboard while it is open (doc "Changing
// keybindings"): no binding fires behind it, so tab walks its controls and a
// letter typed on a focused button does not switch the workspace underneath;
// escape closes it.

import { describe, expect, test } from 'vitest';
import { settingsViewKey } from '../src/lib/settingsKeys';

describe('settingsViewKey', () => {
  test('closed: every key goes to the keybindings', () => {
    expect(settingsViewKey(false, 'Tab')).toBeNull();
    expect(settingsViewKey(false, 'Escape')).toBeNull();
    expect(settingsViewKey(false, 'n')).toBeNull();
  });

  test('open: escape closes the view', () => {
    expect(settingsViewKey(true, 'Escape')).toBe('close');
  });

  test('open: any other key is left to the view', () => {
    for (const key of ['Tab', 'Enter', 'n', '1', ':', ' ']) {
      expect(settingsViewKey(true, key)).toBe('own');
    }
  });
});
