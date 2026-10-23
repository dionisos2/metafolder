// The shell's copy of the global GUI variables (doc "Global GUI variables").
// Rust holds them; this copy is loaded once and moved by every
// `global-var-changed` event, so a panel can answer at once what one holds —
// a right-click menu is built synchronously and greys "Paste" out on an empty
// clipboard. It is the last value announced: a command reads through
// `metafolder.global.get`, in its transaction, never from here.

const values = new Map<string, unknown>();

export const globalMirror = {
  /** The value last announced for `key`; `null` when there is none. */
  current(key: string): unknown {
    return values.has(key) ? values.get(key) : null;
  },
  /** Fills the copy from every variable, as `global_vars` answers them — but
   *  for those an event announced meanwhile, which are newer. */
  load(vars: Record<string, unknown>): void {
    for (const [key, value] of Object.entries(vars)) if (!values.has(key)) values.set(key, value);
  },
  /** One `global-var-changed` event. */
  apply(key: string, value: unknown): void {
    values.set(key, value);
  },
  /** Empties the copy (tests). */
  reset(): void {
    values.clear();
  },
};
