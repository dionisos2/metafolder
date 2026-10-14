// The list's zone commands registered on their own: the zones arrive as a
// table, so `focus` / `clear` / `apply` / `insert` are driven without
// mounting the panel.

import { describe, expect, test, vi } from 'vitest';
import { registerZoneCommands } from '../../default-config/panel-types/metarecord-list/zone-commands.js';

type Spec = { handler: (...args: string[]) => Promise<unknown> };

function setup() {
  const registered = new Map<string, Spec>();
  const host = document.createElement('div');
  const root = host.attachShadow({ mode: 'open' });
  const input = document.createElement('input');
  root.append(input);
  document.body.append(host);
  const zone = {
    input,
    enter: vi.fn(async () => {}),
    apply: vi.fn(async () => {}),
    clear: vi.fn(async () => {
      input.value = '';
    }),
  };
  const clearAll = vi.fn(async () => {});
  registerZoneCommands({
    commands: {
      register: async (name: string, spec: Spec) => void registered.set(name, spec),
    } as never,
    root,
    zones: { simplified: zone },
    clearAll,
  });
  const run = (name: string, ...args: string[]) => registered.get(name)!.handler(...args);
  return { run, zone, input, root, clearAll };
}

describe('the zone commands', () => {
  test('insert splices the whole trailing text at the caret, after entering the zone', async () => {
    const { run, zone, input } = setup();
    input.value = 'ab';
    input.setSelectionRange(1, 1);
    const typed = vi.fn();
    input.addEventListener('input', typed);
    await run('metarecord-list:insert', 'simplified', '#=jazz', 'rock');
    expect(zone.enter).toHaveBeenCalled();
    expect(input.value).toBe('a#=jazz rockb');
    expect(typed).toHaveBeenCalled();
  });

  test('insert with stay leaves the focus where it is', async () => {
    const { run, input, root } = setup();
    await run('metarecord-list:insert', 'simplified', 'x', 'stay');
    expect(input.value).toBe('x');
    expect(root.activeElement).not.toBe(input);
  });

  test('apply commits the zone and gives up its focus', async () => {
    const { run, zone, input, root } = setup();
    input.focus();
    await run('metarecord-list:apply', 'simplified');
    expect(zone.apply).toHaveBeenCalled();
    expect(root.activeElement).not.toBe(input);
  });

  test('clear all goes to the panel', async () => {
    const { run, clearAll, zone } = setup();
    await run('metarecord-list:clear', 'all');
    expect(clearAll).toHaveBeenCalled();
    expect(zone.clear).not.toHaveBeenCalled();
  });

  test('refuses a zone it does not know', async () => {
    const { run } = setup();
    await expect(run('metarecord-list:focus', 'nowhere')).rejects.toThrow(/unknown zone/);
  });
});
