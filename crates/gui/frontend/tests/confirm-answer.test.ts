// Answering confirmations for a scripted command (`mf gui command --confirm`,
// doc "GUI scripting endpoints"): while such a command runs, every
// `window.confirm` it asks is answered for it and its question recorded; outside
// one, the dialog is the user's, untouched.

import { describe, expect, test, vi } from 'vitest';
import { createConfirmAnswerer } from '../src/lib/confirm';

/** A window whose native confirm records its questions and answers `native`. */
function fakeWindow(native = false) {
  const asked: string[] = [];
  const win = {
    confirm: vi.fn((question?: string) => {
      asked.push(question ?? '');
      return native;
    }),
  };
  return { win, asked };
}

describe('createConfirmAnswerer', () => {
  test('a command with an answer gets it, and the questions are reported in order', async () => {
    const { win, asked } = fakeWindow();
    const answerer = createConfirmAnswerer(win);

    const { result, questions } = await answerer.answering(true, async () => {
      const first = win.confirm('Trash a.txt?');
      await Promise.resolve();
      const second = win.confirm('Trash b.txt?');
      return [first, second];
    });

    expect(result).toEqual([true, true]);
    expect(questions).toEqual(['Trash a.txt?', 'Trash b.txt?']);
    expect(asked).toEqual([]); // the native dialog never opened
  });

  test('a refusal is answered as such', async () => {
    const { win } = fakeWindow(true);
    const answerer = createConfirmAnswerer(win);
    const { result } = await answerer.answering(false, async () => win.confirm('Delete?'));
    expect(result).toBe(false);
  });

  test('without an answer, and after the command, the dialog is the native one', async () => {
    const { win, asked } = fakeWindow(true);
    const answerer = createConfirmAnswerer(win);

    const { result, questions } = await answerer.answering(null, async () => win.confirm('Q1?'));
    expect(result).toBe(true);
    expect(questions).toEqual([]);

    await answerer.answering(false, async () => undefined);
    expect(win.confirm('Q2?')).toBe(true);
    expect(asked).toEqual(['Q1?', 'Q2?']);
  });

  test('the answer stops with the command, even when it throws', async () => {
    const { win, asked } = fakeWindow(false);
    const answerer = createConfirmAnswerer(win);

    await expect(
      answerer.answering(true, async () => {
        throw new Error('boom');
      }),
    ).rejects.toThrow('boom');
    win.confirm('later?');
    expect(asked).toEqual(['later?']);
  });

  test('two answering commands at once are refused rather than mixed up', async () => {
    const { win } = fakeWindow();
    const answerer = createConfirmAnswerer(win);
    let release: () => void = () => {};
    const first = answerer.answering(true, () => new Promise<void>((r) => (release = r)));

    await expect(answerer.answering(false, async () => undefined)).rejects.toThrow(
      /another command/,
    );
    // A command without an answer is not concerned.
    await expect(answerer.answering(null, async () => 'ok')).resolves.toMatchObject({
      result: 'ok',
    });
    release();
    await first;
  });
});
