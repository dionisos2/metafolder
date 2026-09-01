// A one-shot signal carried by a monotone counter in the store (the shell asks
// the command input to take the focus by bumping `ui.commandInputFocusTick`).
//
// The reader lives in a component that is destroyed and re-created — leaving
// fullscreen rebuilds the whole chrome, CommandInput included — while the
// counter lives in the module-level store and keeps its value. A gate remembers
// the value it was created with, so a re-created component reacts to the next
// *change* only, and never to a bump that happened before it existed.

/**
 * @param initial the counter's value when the reader is created
 * @returns a predicate: true exactly once per change of the counter
 */
export function createTickGate(initial: number): (tick: number) => boolean {
  let seen = initial;
  return (tick: number) => {
    if (tick === seen) return false;
    seen = tick;
    return true;
  };
}
