// Macros (doc "Macros"): the recording and the last macro recorded, kept in
// the shell for the session. `dispatch` feeds the recorder (commands.ts); the
// `macro:*` commands read it.
//
// A step is an invocation as `mf.invoke` takes it — the name, then one element
// per argument, `ask` where the argument is to be asked again — so replaying a
// macro and saving it as a user command are one and the same thing.

import { type AskMarker, ask, type InvocationParts, isAsk } from './commands';
import { store } from './store.svelte';

export type MacroStep = InvocationParts;

let recording: MacroStep[] | null = null;
let last: MacroStep[] | null = null;
// The question on screen for a command being recorded: which argument it
// asks for, and the marks of that command (`macro:ask`).
let prompt: { marks: Set<number>; position: number } | null = null;

export const macros = {
  recording: () => recording !== null,
  last: () => last,
  /** For tests: a macro as if recorded. */
  setLast(steps: MacroStep[] | null) {
    last = steps;
  },

  /** `macro:toggle-recording`: starts a recording, or ends it and keeps it
   *  as the last macro — unless it recorded nothing. Returns the steps
   *  recorded when it ended one, null when it started one. */
  toggle(): MacroStep[] | null {
    if (recording === null) {
      recording = [];
      store.ui.macroRecording = true;
      return null;
    }
    const done = recording;
    recording = null;
    prompt = null;
    store.ui.macroRecording = false;
    if (done.length > 0) last = done;
    return done;
  },

  /** A command that ran, recorded with the arguments it ran with, those
   *  marked by `macro:ask` asked again. */
  record(head: string, args: string[], marks: ReadonlySet<number>) {
    if (recording === null) return;
    recording.push([head, ...args.map((arg, i): string | AskMarker => (marks.has(i) ? ask : arg))]);
  },

  /** Brackets a question of a command being recorded. */
  async asking<T>(marks: Set<number>, position: number, question: () => Promise<T>): Promise<T> {
    const mine = { marks, position };
    prompt = mine;
    try {
      return await question();
    } finally {
      if (prompt === mine) prompt = null;
    }
  },

  /** `macro:ask`: marks — or unmarks — the argument on screen. Returns
   *  whether it is now marked, null when there is no such question. */
  mark(): boolean | null {
    if (recording === null || prompt === null) return null;
    const { marks, position } = prompt;
    if (marks.has(position)) {
      marks.delete(position);
      return false;
    }
    marks.add(position);
    return true;
  },
};

/** For tests: no recording, no macro. */
export function resetMacros(): void {
  recording = null;
  last = null;
  prompt = null;
  store.ui.macroRecording = false;
}

/** A string as a single-quoted JavaScript literal. */
function literal(value: string): string {
  const escaped = value
    .replace(/\\/g, '\\\\')
    .replace(/'/g, "\\'")
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\u2028/g, '\\u2028')
    .replace(/\u2029/g, '\\u2029');
  return `'${escaped}'`;
}

/** The entry of `macros.js` for a macro: the user command one would write by
 *  hand, one `mf.invoke` per step (doc "Macros"). */
export function macroEntry(name: string, steps: MacroStep[]): string {
  const lines = [
    `  ${literal(name)}: {`,
    `    label: ${literal(`Macro ${name}`)},`,
    '    run: async (mf) => {',
    ...steps.map(
      (step) =>
        `      await mf.invoke([${step.map((part) => (isAsk(part) ? 'mf.ask' : literal(part))).join(', ')}]);`,
    ),
    '    },',
    '  },',
    '',
  ];
  return lines.join('\n');
}
