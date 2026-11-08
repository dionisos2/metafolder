// A panel instance torn down while its mount is in flight (PanelHost.svelte):
// its workspace closed before the panel's files arrived — a script stopped
// right after opening it. The mount finishing afterwards must not leave a
// panel running nobody will clean up.

import { describe, expect, test, vi } from 'vitest';
import { createPanelLifecycle } from '../src/lib/panels/lifecycle';

describe('panel lifecycle', () => {
  test('a cleanup returned by the mount runs at the teardown', () => {
    const life = createPanelLifecycle();
    const cleanup = vi.fn();
    life.mounted(cleanup);
    expect(cleanup).not.toHaveBeenCalled();
    life.dispose();
    expect(cleanup).toHaveBeenCalledTimes(1);
    life.dispose();
    expect(cleanup).toHaveBeenCalledTimes(1);
  });

  test('a mount ending after the teardown is cleaned up at once', () => {
    const life = createPanelLifecycle();
    expect(life.alive()).toBe(true);
    life.dispose();
    expect(life.alive()).toBe(false);
    const cleanup = vi.fn();
    life.mounted(cleanup);
    expect(cleanup).toHaveBeenCalledTimes(1);
  });

  test('a mount returning no function, or a throwing cleanup, is fine', () => {
    const life = createPanelLifecycle();
    life.mounted(undefined);
    expect(() => life.dispose()).not.toThrow();
    const late = createPanelLifecycle();
    late.dispose();
    expect(() =>
      late.mounted(() => {
        throw new Error('boom');
      }),
    ).not.toThrow();
  });
});
