// Whether the task bar and the question bar are shown (doc "Status bar and task
// bar"). Both take their own place in the layout, between the panels and the
// command input, so each appearance or disappearance resizes every panel. They
// therefore move only when something new is to be read, or when the user asks:
//
//  - a bar shows from its first entry on, and stays once its entries are gone
//    (an empty line), so the panels do not jump as entries come and go;
//  - `bar:toggle <name>` hides it, or shows it again;
//  - a new entry — one that was not in the bar at the last look — shows it again
//    even if hidden: a new task, a new question is something to read.
//
// The entries are named by the caller (task ids, a question's id); this module
// only compares the names of one look with those of the previous one.

export const BAR_NAMES = ['tasks', 'question'] as const;
export type BarName = (typeof BAR_NAMES)[number];
export type BarsState = Record<BarName, boolean>;

export function isBarName(name: string): name is BarName {
  return (BAR_NAMES as readonly string[]).includes(name);
}

export function createBars() {
  const state: BarsState = { tasks: false, question: false };
  const seen: Record<BarName, Set<string>> = { tasks: new Set(), question: new Set() };
  const listeners = new Set<(state: BarsState) => void>();

  function set(name: BarName, shown: boolean) {
    if (state[name] === shown) return;
    state[name] = shown;
    for (const listener of listeners) listener({ ...state });
  }

  return {
    shown: (name: BarName): boolean => state[name],
    /** What the bar `name` holds now: shows it if any entry is new. */
    note(name: BarName, entries: readonly string[]) {
      const previous = seen[name];
      const fresh = entries.some((entry) => !previous.has(entry));
      seen[name] = new Set(entries);
      if (fresh) set(name, true);
    },
    toggle(name: BarName) {
      set(name, !state[name]);
    },
    subscribe(listener: (state: BarsState) => void): () => void {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** The GUI's one pair of bars. */
export const bars = createBars();
