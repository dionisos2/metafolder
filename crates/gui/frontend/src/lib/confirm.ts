// Answering confirmations for a scripted command (doc "GUI scripting
// endpoints"): `mf gui command --confirm yes|no` runs a command whose
// `window.confirm` dialogs are answered for it, so a script — or the GUI's
// end-to-end tests — can drive a confirmed command such as `metarecord:remove`.
//
// The answer is tied to the command, never armed in advance: it holds while the
// command runs and stops when it resolves, so an answer nobody asked for cannot
// linger and accept a dialog the user opens later. Panels run in the shell's
// realm and call the global `confirm`, so replacing `window.confirm` once covers
// every dialog of the GUI.

interface ConfirmWindow {
  confirm(question?: string): boolean;
}

export interface ConfirmAnswerer {
  /**
   * Runs `run`, answering every confirmation it asks with `answer` (`null`:
   * leave them to the user). Resolves with its result and the questions
   * answered, in order. Refused while another answering command runs: the two
   * answers could not be told apart.
   */
  answering<T>(
    answer: boolean | null,
    run: () => Promise<T>,
  ): Promise<{ result: T; questions: string[] }>;
}

export function createConfirmAnswerer(win: ConfirmWindow): ConfirmAnswerer {
  const native = win.confirm.bind(win);
  let active: { answer: boolean; questions: string[] } | null = null;

  win.confirm = (question?: string) => {
    if (!active) return native(question);
    active.questions.push(String(question ?? ''));
    return active.answer;
  };

  return {
    async answering(answer, run) {
      if (answer === null) return { result: await run(), questions: [] };
      if (active) throw new Error('another command is answering confirmations');
      const current = { answer, questions: [] as string[] };
      active = current;
      try {
        return { result: await run(), questions: current.questions };
      } finally {
        active = null;
      }
    },
  };
}
