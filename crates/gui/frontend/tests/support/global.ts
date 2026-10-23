// A stand-in for `metafolder.global` (doc "Global GUI variables"): one store,
// written at once, which `current` reads as the shell's copy would after the
// change was announced.

import { vi } from 'vitest';

export function globalStub(initial: Record<string, unknown> = {}) {
  const values = new Map<string, unknown>(Object.entries(initial));
  const read = (key: string) => (values.has(key) ? values.get(key) : null);
  return {
    values,
    get: vi.fn(async (key: string) => read(key)),
    set: vi.fn(async (key: string, value: unknown) => void values.set(key, value)),
    setMany: vi.fn(async (vars: Record<string, unknown>) => {
      for (const [key, value] of Object.entries(vars)) values.set(key, value);
    }),
    all: vi.fn(async () => Object.fromEntries(values)),
    current: read,
    onChange: vi.fn(),
  };
}
