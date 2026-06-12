// Visibility gate (panel-shim/visibility.js): panels defer their first
// data load until actually displayed, so pre-instantiated hidden panels
// cost nothing beyond command registration.

import { describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel shim
import { createVisibilityGate } from '../../panel-shim/visibility.js';

describe('createVisibilityGate', () => {
  test('starts hidden; whenVisible defers until shown', () => {
    const gate = createVisibilityGate();
    const fn = vi.fn();
    expect(gate.visible).toBe(false);
    gate.whenVisible(fn);
    expect(fn).not.toHaveBeenCalled();
    gate.set(true);
    expect(gate.visible).toBe(true);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  test('whenVisible runs immediately when already visible', () => {
    const gate = createVisibilityGate();
    gate.set(true);
    const fn = vi.fn();
    gate.whenVisible(fn);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  test('a pending callback fires once, not on every later show', () => {
    const gate = createVisibilityGate();
    const fn = vi.fn();
    gate.whenVisible(fn);
    gate.set(true);
    gate.set(false);
    gate.set(true);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  test('registering the same function twice while hidden runs it once', () => {
    // Panels re-arm with a stable callback (e.g. on active_repo changes
    // while hidden): the load must not run twice on the first show.
    const gate = createVisibilityGate();
    const fn = vi.fn();
    gate.whenVisible(fn);
    gate.whenVisible(fn);
    gate.set(true);
    expect(fn).toHaveBeenCalledTimes(1);
  });
});
