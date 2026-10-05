// shell panel: what the shell lines run in the workspace printed (doc "shell
// panel"). Read like a terminal: oldest at the top, one block per run with its
// command line, stderr told apart, how it ended when worth saying — and two
// runs at once each keep their own block.

import { describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve as resolvePath } from 'node:path';

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

async function mounted(history: Entry[]) {
  const shadow = shadowFor();
  let push: (entry: unknown) => void = () => {};
  const invoke = vi.fn(async () => {});
  const api = {
    commands: { invoke },
    shell: {
      list: async () => history,
      onAppend: (listener: (entry: unknown) => void) => {
        push = listener;
      },
    },
  };
  const mod = await import('../../default-config/panel-types/shell/main.js');
  await mod.mount(shadow, api as never);
  return { shadow, push: (e: unknown) => push(e), invoke };
}

/** Each run as [command, ...lines], lines prefixed by their kind. */
function blocks(shadow: ShadowRoot) {
  return [...shadow.querySelectorAll('.run')].map((run) => [
    run.querySelector('.command')!.textContent,
    ...[...run.querySelectorAll('.out')].map((l) => `${l.className.replace('out ', '')}:${l.textContent}`),
  ]);
}

describe('shell panel', () => {
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
    expect(invoke).toHaveBeenCalledWith('shell:clear');
  });
});
