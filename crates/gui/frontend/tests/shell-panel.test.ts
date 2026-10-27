// shell panel: what the shell lines run in the workspace printed (doc "shell
// panel"). Read like a terminal: oldest at the top, one block per run with its
// command line, stderr told apart, how it ended when worth saying — and two
// runs at once each keep their own block.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve as resolvePath } from 'node:path';
import { withRuns } from './support/runs';

const PANEL_DIR = resolvePath(process.cwd(), '../default-config/panel-types/shell');

function shadowFor(): ShadowRoot {
  const html = readFileSync(resolvePath(PANEL_DIR, 'index.html'), 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const host = document.createElement('div');
  document.body.append(host);
  const shadow = host.attachShadow({ mode: 'open' });
  const body = document.createElement('div');
  body.className = 'mf-panel-body';
  for (const child of [...doc.body.childNodes]) {
    if (child.nodeName === 'SCRIPT' || child.nodeName === 'STYLE') continue;
    body.append(child);
  }
  shadow.append(body);
  return shadow;
}

type Entry = { ts_ms: number; run: string; kind: string; text: string };
const entry = (run: string, kind: string, text: string): Entry => ({ ts_ms: 0, run, kind, text });

type Completion = { text: string; cursor: number; candidates: string[] };

async function mounted(history: Entry[], completion?: Completion) {
  const shadow = shadowFor();
  let push: (entry: unknown) => void = () => {};
  let cwdMoved: () => void = () => {};
  const changes = new Map<string, () => void>();
  const where = { dir: '/data/music' as string | null };
  const invoke = vi.fn(async (_invocation: string) => {});
  const top = vi.fn(async (_invocation: string) => {});
  const complete = vi.fn(async (_line: string, _cursor: number) => completion);
  const appended: string[] = [];
  // Whether a line runs in the workspace's shell: what is typed is then its input.
  const shellState = { running: false };
  const input = vi.fn(async (_text: string | null) => shellState.running);
  const interrupt = vi.fn(async () => shellState.running);
  const handlers = new Map<string, (...args: string[]) => unknown>();
  const api = {
    invoke: top,
    commands: {
      invoke,
      register: async (name: string, options: { handler: (...args: string[]) => unknown }) => {
        handlers.set(name, options.handler);
      },
    },
    workspace: {
      get: async () => 'repo-1',
      onChange: (key: string, listener: () => void) => changes.set(key, listener),
    },
    history: {
      read: async () => ['older', 'newest'],
      append: async (_repo: string, _zone: string, entry: string) => {
        appended.push(entry);
      },
    },
    shell: {
      list: async () => history,
      onAppend: (listener: (entry: unknown) => void) => {
        push = listener;
      },
      complete,
      input,
      interrupt,
      cwd: async () => where.dir,
      onCwd: (listener: () => void) => {
        cwdMoved = listener;
      },
    },
  };
  const mod = await import('../../default-config/panel-types/shell/main.js');
  await mod.mount(shadow, withRuns(api) as never);
  const line = shadow.getElementById('line') as HTMLInputElement;
  return {
    shadow,
    push: (e: unknown) => push(e),
    invoke,
    top,
    complete,
    appended,
    handlers,
    shellState,
    input,
    interrupt,
    line,
    where,
    cwdMoved: () => cwdMoved(),
    select: () => changes.get('selected_paths')?.(),
  };
}

function type(line: HTMLInputElement, text: string) {
  line.focus();
  line.value = text;
  line.setSelectionRange(text.length, text.length);
  line.dispatchEvent(new Event('input', { bubbles: true }));
}

function press(line: HTMLInputElement, key: string, init: KeyboardEventInit = {}) {
  const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...init });
  line.dispatchEvent(event);
  return event;
}

const settle = () => new Promise((r) => setTimeout(r, 0));

/** Each run as [command, ...lines], lines prefixed by their kind. */
function blocks(shadow: ShadowRoot) {
  return [...shadow.querySelectorAll('.run')].map((run) => [
    run.querySelector('.command')!.textContent,
    ...[...run.querySelectorAll('.out')].map((l) => `${l.className.replace('out ', '')}:${l.textContent}`),
  ]);
}

describe('shell panel', () => {
  test('the prompt shows where the next line runs, asked again when it may move', async () => {
    const { shadow, where, cwdMoved, select } = await mounted([]);
    await settle();
    const cwd = () => shadow.getElementById('cwd')!.textContent.replace('\u200e', '');
    expect(cwd()).toBe('/data/music');
    where.dir = '/data';
    cwdMoved();
    await settle();
    expect(cwd()).toBe('/data');
    where.dir = '/data/films';
    select();
    await settle();
    expect(cwd()).toBe('/data/films');
    where.dir = null;
    cwdMoved();
    await settle();
    expect(cwd()).toBe('');
  });

  test('shows the history oldest first, one block per run', async () => {
    const { shadow } = await mounted([
      entry('script-1', 'command', 'ls'),
      entry('script-1', 'stdout', 'a'),
      entry('script-1', 'stdout', 'b'),
      entry('script-2', 'command', 'false'),
      entry('script-2', 'status', 'exit 1'),
    ]);
    expect(blocks(shadow)).toEqual([
      ['ls', 'stdout:a', 'stdout:b'],
      ['false', 'status:exit 1'],
    ]);
  });

  test('a line goes into its own run, even when runs interleave', async () => {
    const { shadow, push } = await mounted([]);
    push(entry('script-1', 'command', 'slow'));
    push(entry('script-2', 'command', 'fast'));
    push(entry('script-2', 'stderr', 'oops'));
    push(entry('script-1', 'stdout', 'done'));
    expect(blocks(shadow)).toEqual([
      ['slow', 'stdout:done'],
      ['fast', 'stderr:oops'],
    ]);
  });

  test('a null entry clears the log', async () => {
    const { shadow, push } = await mounted([entry('script-1', 'command', 'ls')]);
    push(null);
    expect(blocks(shadow)).toEqual([]);
    // A run seen before the clear starts a fresh block if it prints again.
    push(entry('script-1', 'stdout', 'late'));
    expect(shadow.querySelectorAll('.run')).toHaveLength(1);
  });

  test('Clear runs shell:clear', async () => {
    const { shadow, invoke } = await mounted([]);
    (shadow.getElementById('clear') as HTMLButtonElement).click();
    expect(invoke).toHaveBeenCalledWith(['shell:clear']);
  });

  describe('following the output', () => {
    /** Gives the log a geometry (jsdom lays nothing out). */
    function geometry(log: HTMLElement, scrollHeight: number, clientHeight: number) {
      Object.defineProperty(log, 'scrollHeight', { value: scrollHeight, configurable: true });
      Object.defineProperty(log, 'clientHeight', { value: clientHeight, configurable: true });
    }

    test('keeps following when the log shrank under its last line', async () => {
      const { shadow, push } = await mounted([]);
      const log = shadow.getElementById('log') as HTMLElement;
      geometry(log, 500, 100);
      log.scrollTop = 400;
      log.dispatchEvent(new Event('scroll'));
      // The prompt grew a line (the cwd shown): the bottom is now 20px
      // below the view, and no scroll happened.
      geometry(log, 500, 80);
      push(entry('script-1', 'stdout', 'next'));
      expect(log.scrollTop).toBe(500);
    });

    test('a reader who scrolled up keeps their place', async () => {
      const { shadow, push } = await mounted([]);
      const log = shadow.getElementById('log') as HTMLElement;
      geometry(log, 500, 100);
      log.scrollTop = 400;
      log.dispatchEvent(new Event('scroll'));
      log.scrollTop = 100;
      log.dispatchEvent(new Event('scroll'));
      push(entry('script-1', 'stdout', 'next'));
      expect(log.scrollTop).toBe(100);
      // Back at the bottom, it follows again.
      log.scrollTop = 395;
      log.dispatchEvent(new Event('scroll'));
      geometry(log, 520, 100);
      push(entry('script-1', 'stdout', 'more'));
      expect(log.scrollTop).toBe(520);
    });

    test('a resized log goes back to the bottom while following', async () => {
      const observers: Array<() => void> = [];
      vi.stubGlobal(
        'ResizeObserver',
        class {
          constructor(callback: () => void) {
            observers.push(callback);
          }
          observe() {}
          disconnect() {}
        },
      );
      try {
        const { shadow } = await mounted([]);
        const log = shadow.getElementById('log') as HTMLElement;
        // Shown again after being hidden, where no scroll could take.
        geometry(log, 500, 100);
        log.scrollTop = 0;
        for (const callback of observers) callback();
        expect(log.scrollTop).toBe(500);
      } finally {
        vi.unstubAllGlobals();
      }
    });
  });

  describe('the command line', () => {
    afterEach(() => document.body.replaceChildren());

    test('Enter runs the typed line as a shell line, and empties the field', async () => {
      const { line, top } = await mounted([]);
      type(line, 'ls -l %');
      expect(press(line, 'Enter').defaultPrevented).toBe(true);
      await settle();
      // `!`: the same dispatch as the command input's bash mode, `%` expansion included.
      expect(top).toHaveBeenCalledWith(['!ls -l %']);
      expect(line.value).toBe('');
    });

    test('while a line runs, Enter hands the text to it, and runs nothing', async () => {
      const { line, top, input, appended, shellState } = await mounted([]);
      shellState.running = true;
      type(line, 'yes');
      press(line, 'Enter');
      await settle();
      expect(input).toHaveBeenCalledWith('yes');
      expect(top).not.toHaveBeenCalled();
      // An answer to a program is not a shell line: not in the history.
      expect(appended).toEqual([]);
      expect(line.value).toBe('');
    });

    test('ctrl+d on an empty field ends the running line\'s input', async () => {
      const { line, input } = await mounted([]);
      line.focus();
      expect(press(line, 'd', { ctrlKey: true }).defaultPrevented).toBe(true);
      expect(input).toHaveBeenCalledWith(null);
      type(line, 'abc');
      expect(press(line, 'd', { ctrlKey: true }).defaultPrevented).toBe(false);
    });

    test('ctrl+c interrupts the running line, unless text is selected to copy', async () => {
      const { line, interrupt } = await mounted([]);
      type(line, 'abc');
      expect(press(line, 'c', { ctrlKey: true }).defaultPrevented).toBe(true);
      expect(interrupt).toHaveBeenCalledTimes(1);
      line.setSelectionRange(0, 2);
      expect(press(line, 'c', { ctrlKey: true }).defaultPrevented).toBe(false);
      expect(interrupt).toHaveBeenCalledTimes(1);
    });

    test('the field keeps the keyboard after a run, as in a terminal', async () => {
      const { line, shadow } = await mounted([]);
      type(line, 'ls');
      press(line, 'Enter');
      expect(shadow.activeElement).toBe(line);
    });

    test('a blank line runs nothing', async () => {
      const { line, top } = await mounted([]);
      type(line, '   ');
      press(line, 'Enter');
      expect(top).not.toHaveBeenCalled();
    });

    test('what ran goes into the bash history, shared with the command input', async () => {
      const { line, appended } = await mounted([]);
      type(line, 'echo hi');
      press(line, 'Enter');
      await settle();
      expect(appended).toEqual(['echo hi']);
    });

    test('ctrl+p recalls the previous line', async () => {
      const { line } = await mounted([]);
      line.focus();
      press(line, 'p', { ctrlKey: true });
      await settle();
      expect(line.value).toBe('newest');
    });

    test('running a line follows the output again, even after scrolling up', async () => {
      const { line, shadow } = await mounted([]);
      const log = shadow.getElementById('log') as HTMLElement;
      Object.defineProperty(log, 'scrollHeight', { value: 500, configurable: true });
      log.scrollTop = 0;
      type(line, 'ls');
      press(line, 'Enter');
      expect(log.scrollTop).toBe(500);
    });

    test('Tab completes the word before the cursor', async () => {
      const { line, complete } = await mounted([], {
        text: 'ls Documents/',
        cursor: 13,
        candidates: [],
      });
      type(line, 'ls Doc');
      expect(press(line, 'Tab').defaultPrevented).toBe(true);
      await settle();
      expect(complete).toHaveBeenCalledWith('ls Doc', 6);
      expect(line.value).toBe('ls Documents/');
      expect(line.selectionStart).toBe(13);
    });

    test('several candidates are listed under the output, until the next edit', async () => {
      const { line, shadow } = await mounted([], {
        text: 'cat foo',
        cursor: 7,
        candidates: ['foo', 'foobar'],
      });
      type(line, 'cat f');
      press(line, 'Tab');
      await settle();
      const candidates = shadow.getElementById('candidates') as HTMLElement;
      expect(candidates.hidden).toBe(false);
      expect(candidates.textContent).toContain('foobar');
      type(line, 'cat foob');
      expect(candidates.hidden).toBe(true);
    });

    test('shell:focus line gives the field the keyboard', async () => {
      const { line, shadow, handlers } = await mounted([]);
      await handlers.get('shell:focus')!('line');
      expect(shadow.activeElement).toBe(line);
    });
  });
});
