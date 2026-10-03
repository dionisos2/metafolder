// Pure logic of the command input: invocation parsing and autocomplete
// filtering (lib/commands.ts). Dispatch itself talks to Tauri and is
// exercised in the running app.

import { afterEach, describe, expect, test, vi } from 'vitest';
import {
  type ArgPromptRequest,
  type ArgSpec,
  argSpecFor,
  discardActiveInput,
  clearArgSpecs,
  collectArgs,
  clearUserCommands,
  deadInvocations,
  dispatch,
  dropUserCommand,
  filterCommands,
  filterCompletionItems,
  installUserCommands,
  filterCompletions,
  listedCommands,
  needsMessagePanel,
  parseInvocation,
  promptsForInput,
  registerArgs,
  setPanelArgs,
  resolvePromptValue,
  runUserCommand,
  validateUserCommands,
  withTopLevelInvoke,
  resolveSubmission,
  shortcutsFor,
  shouldLogCommand,
} from '../src/lib/commands';
import type { CompletionFn } from '../src/lib/completions';
import type { LayoutView } from '../src/lib/types';

// Runs before any clearArgSpecs() (called in the argSpecs-registry suite below):
// the `recent` builtin registers its argument spec at module load, so the
// command input collects the metarecord pick interactively.
describe('config:reload argument spec', () => {
  test('completes over the reloadable targets plus `all`', async () => {
    const spec = argSpecFor('config:reload');
    expect(spec).toHaveLength(1);
    expect(await (spec![0].complete as CompletionFn)('', [])).toEqual([
      'keybindings',
      'style',
      'grammar',
      'commands',
      'all',
    ]);
  });

  test('the target is optional: invoked bare it asks, never refuses', async () => {
    // Bare `config:reload` is the discoverable form; it must prompt rather
    // than fail, which is what the ellipsis in the listing promises.
    expect(promptsForInput('config:reload')).toBe(true);
    expect(promptsForInput('config:reload grammar')).toBe(false);
  });
});

// `recent` is a shipped `commands.js` entry since it left the shell builtins:
// its `metarecord` argument collects the pick interactively (completing over
// the recently-viewed list). The lines the pick offers and the way it opens
// are recent.test.ts.
describe('recent (shipped commands.js)', () => {
  test('declares its metarecord argument', async () => {
    const shipped = (await import('../../default-config/commands.js')).default;
    const spec = shipped['recent'].args;

    expect(spec.map((a: { name: string }) => a.name)).toEqual(['metarecord']);
    expect(spec[0].prompt()).toBe('Recently viewed:');
  });
});

// `mf:order` asks nothing: it numbers the selected folder (or the selected
// file's folder) straight away (doc "Ordering a folder").
describe('mf:order builtin', () => {
  test('takes no argument, so it runs without a prompt', () => {
    expect(argSpecFor('mf:order')).toBeUndefined();
    expect(promptsForInput('mf:order')).toBe(false);
  });
});

// `repos:switch` is a shipped `commands.js` entry since it left the shell
// builtins: its `repo` argument collects the repository pick interactively
// (completing over the daemon's loaded repositories as "<name> — <root>"
// lines), and picking one opens it exactly like a click in the repos panel —
// adopted in the focused workspace when it has no repository yet, otherwise a
// new workspace. The flow itself is repos-switch.test.ts.
describe('repos:switch (shipped commands.js)', () => {
  test('declares its repo argument, with completion', async () => {
    const shipped = (await import('../../default-config/commands.js')).default;
    const spec = shipped['repos:switch'].args;

    expect(spec.map((a: { name: string }) => a.name)).toEqual(['repo']);
    expect(spec[0].prompt()).toBe('Open repository:');
  });
});

// `editing:discard` (ctrl+q) on a PANEL input: the shell command input handles
// it through its registered editing target, but a panel's own input (the
// metarecord-list query box, a metarecord-detail inline editor…) has no such
// target, so the command falls back to the deep-focused element — doc
// "editing:unfocus": clear the active text input, then remove focus.
describe('discardActiveInput', () => {
  afterEach(() => document.body.replaceChildren());

  test('clears a focused input, notifies listeners and blurs it', () => {
    const input = document.createElement('input');
    input.value = 'jazz';
    document.body.append(input);
    input.focus();
    const seen: string[] = [];
    input.addEventListener('input', () => seen.push(input.value));

    expect(discardActiveInput(input)).toBe(true);

    expect(input.value).toBe('');
    expect(seen).toEqual(['']); // the panel sees the change (its own listener)
    expect(document.activeElement).not.toBe(input);
  });

  test('clears a textarea the same way', () => {
    const area = document.createElement('textarea');
    area.value = 'two\nlines';
    document.body.append(area);
    area.focus();

    expect(discardActiveInput(area)).toBe(true);
    expect(area.value).toBe('');
  });

  test('a non-text element is only blurred, never emptied', () => {
    const button = document.createElement('button');
    button.textContent = 'keep me';
    document.body.append(button);
    button.focus();

    expect(discardActiveInput(button)).toBe(false);
    expect(button.textContent).toBe('keep me');
    expect(document.activeElement).not.toBe(button);
  });

  test('nothing focused is a no-op', () => {
    expect(discardActiveInput(null)).toBe(false);
  });
});

describe('parseInvocation', () => {
  test('plain command name', () => {
    expect(parseInvocation('workspace:new')).toEqual({ name: 'workspace:new', args: [] });
  });

  test('command with parameters', () => {
    expect(parseInvocation('metarecord-list:set mode grid')).toEqual({
      name: 'metarecord-list:set',
      args: ['mode', 'grid'],
    });
    expect(parseInvocation('answer:send left')).toEqual({
      name: 'answer:send',
      args: ['left'],
    });
  });

  test('double-quoted parameters keep spaces', () => {
    expect(parseInvocation('workspace:rename "My music workspace"')).toEqual({
      name: 'workspace:rename',
      args: ['My music workspace'],
    });
  });

  test('shell invocations start with !', () => {
    expect(parseInvocation('!ls -la /tmp')).toEqual({ shell: 'ls -la /tmp' });
    expect(parseInvocation('!  echo hi ')).toEqual({ shell: 'echo hi' });
  });

  test('blank input parses to null', () => {
    expect(parseInvocation('')).toBeNull();
    expect(parseInvocation('   ')).toBeNull();
    expect(parseInvocation('!')).toBeNull();
  });

  test('extra whitespace is tolerated', () => {
    expect(parseInvocation('  workspace:goto 3  ')).toEqual({ name: 'workspace:goto', args: ['3'] });
  });
});

describe('needsMessagePanel', () => {
  const slot = (visible: boolean, workspace_id: string | null, panel_type: string | null) => ({
    visible,
    workspace_id,
    panel_type,
  });
  const layout = (left: ReturnType<typeof slot>, right: ReturnType<typeof slot>): LayoutView =>
    ({ left, right, focused: 'left' });

  test('needed when no slot of the workspace shows message', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(false, null, null));
    expect(needsMessagePanel(l, 'ws1')).toBe(true);
  });

  test('not needed when the focused slot already shows message', () => {
    const l = layout(slot(true, 'ws1', 'message'), slot(false, null, null));
    expect(needsMessagePanel(l, 'ws1')).toBe(false);
  });

  test('not needed when the other slot already shows message', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(true, 'ws1', 'message'));
    expect(needsMessagePanel(l, 'ws1')).toBe(false);
  });

  test('a hidden message slot does not count', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(false, 'ws1', 'message'));
    expect(needsMessagePanel(l, 'ws1')).toBe(true);
  });

  test('a message slot of another workspace does not count', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(true, 'ws2', 'message'));
    expect(needsMessagePanel(l, 'ws1')).toBe(true);
  });

  test('no focused workspace: never needed', () => {
    const l = layout(slot(false, null, null), slot(false, null, null));
    expect(needsMessagePanel(l, null)).toBe(false);
  });
});

describe('promptsForInput with optional arguments', () => {
  afterEach(() => clearArgSpecs());

  test('a fully-optional tail means the invocation runs as typed', () => {
    registerArgs('p:apply', [
      { name: 'zone', prompt: () => 'Zone?' },
      { name: 'stay', optional: true, prompt: () => 'Stay?' },
    ]);
    expect(promptsForInput('p:apply')).toBe(true); // `zone` is still missing
    expect(promptsForInput('p:apply finder')).toBe(false); // `stay` never asks
    expect(promptsForInput('p:apply finder stay')).toBe(false);
  });
});

describe('installUserCommands', () => {
  afterEach(() => {
    clearArgSpecs();
    clearUserCommands();
  });

  const mf = { tag: 'the api' } as never;

  test('each key becomes a registered command, the key being the name', async () => {
    const registered: [string, string, boolean][] = [];
    const names = await installUserCommands(
      { 'user:a': { label: 'A', run: () => {} }, 'user:b': { run: () => {} } },
      mf,
      async (name, label, log) => void registered.push([name, label, log]),
    );
    expect(names).toEqual(['user:a', 'user:b']);
    // A missing label falls back to the name; `log` defaults to true.
    expect(registered).toEqual([
      ['user:a', 'A', true],
      ['user:b', 'user:b', true],
    ]);
  });

  test('an entry named like a builtin is left out, and the builtin untouched', async () => {
    // The registry refuses the name (`register` answers false): the entry must
    // then install nothing — no handler, and above all no argument spec, which
    // would replace the builtin's own.
    registerArgs('quit', [{ name: 'builtin-arg', prompt: () => 'really?' }]);
    const refused: string[] = [];
    const names = await installUserCommands(
      {
        quit: { args: [{ name: 'mine' }], run: () => {} },
        'user:a': { run: () => {} },
      },
      mf,
      async (name) => name !== 'quit',
      (name) => void refused.push(name),
    );
    expect(names).toEqual(['user:a']);
    expect(refused).toEqual(['quit']);
    expect(await runUserCommand('quit', [])).toBe(false);
    expect(argSpecFor('quit')![0].name).toBe('builtin-arg');
    // A reload forgets what the file installed, and only that.
    expect(clearUserCommands()).toEqual(['user:a']);
    expect(argSpecFor('quit')![0].name).toBe('builtin-arg');
  });

  test("a panel's command takes its name back from an entry", async () => {
    // Panels register after the file is loaded, so the entry is dropped then:
    // its handler, and its argument spec — which a panel command declaring no
    // arguments would otherwise inherit.
    await installUserCommands(
      { 'log:find': { args: [{ name: 'mine' }], run: () => {} }, 'user:a': { run: () => {} } },
      mf,
      async () => {},
    );
    expect(dropUserCommand('log:find')).toBe(true);
    expect(await runUserCommand('log:find', [])).toBe(false);
    expect(argSpecFor('log:find')).toBeUndefined();
    expect(await runUserCommand('user:a', [])).toBe(true);
    // Nothing to drop the second time (each workspace's instance registers).
    expect(dropUserCommand('log:find')).toBe(false);
    expect(dropUserCommand('log:next')).toBe(false);
  });

  test('`run` is called with the api first, then the collected arguments', async () => {
    const seen: unknown[] = [];
    await installUserCommands(
      { 'user:a': { run: (...all: unknown[]) => void seen.push(all) } },
      mf,
      async () => {},
    );
    await runUserCommand('user:a', ['x', 'y']);
    expect(seen).toEqual([[mf, 'x', 'y']]);
  });

  test('argument spec functions are handed the api first', async () => {
    await installUserCommands(
      {
        'user:a': {
          args: [
            {
              name: 'tag',
              prompt: (api: unknown) => `prompt ${(api as { tag: string }).tag}`,
              initial: (api: unknown) => `initial ${(api as { tag: string }).tag}`,
              complete: (api: unknown) => [`complete ${(api as { tag: string }).tag}`],
            },
          ],
          run: () => {},
        },
      },
      mf,
      async () => {},
    );
    const spec = argSpecFor('user:a')!;
    expect(await spec[0].prompt([])).toBe('prompt the api');
    expect(await spec[0].initial!([])).toBe('initial the api');
    expect(await (spec[0].complete as CompletionFn)('', [])).toEqual(['complete the api']);
  });

  test('an entry without a callable `run` is rejected by name', async () => {
    await expect(
      installUserCommands({ 'user:a': { label: 'A' } }, mf, async () => {}),
    ).rejects.toThrow(/user:a/);
  });

  test('a malformed module leaves the installed commands alone', async () => {
    // Found end-to-end: a reload cleared first and imported second, so one bad
    // edit took away every user command until the file parsed again.
    await installUserCommands({ 'user:keep': { run: () => {} } }, mf, async () => {});
    expect(() => validateUserCommands({ 'user:bad': { label: 'no run' } })).toThrow(/user:bad/);
    expect(await runUserCommand('user:keep', [])).toBe(true);
  });

  test('a module whose default export is not an object is rejected', async () => {
    await expect(installUserCommands(null, mf, async () => {})).rejects.toThrow(/object/);
    await expect(installUserCommands([], mf, async () => {})).rejects.toThrow(/object/);
  });

  test('the api a user command gets can invoke other commands directly', async () => {
    // `mf.invoke` is the whole point of a user command — it composes existing
    // commands — so it must be on the object, not only under `mf.commands`.
    const calls: string[] = [];
    const api = { commands: { invoke: (i: string) => void calls.push(i) } };
    const withInvoke = withTopLevelInvoke(api);
    withInvoke.invoke('metarecord-list:apply simplified');
    expect(calls).toEqual(['metarecord-list:apply simplified']);
  });

  test('runUserCommand reports an unknown name rather than pretending', async () => {
    expect(await runUserCommand('user:nope', [])).toBe(false);
  });
});

describe('deadInvocations', () => {
  const binding = (keys: string[], invocation: string) => ({ keys, invocation });
  const command = (name: string) => ({ name, label: name, owner: null, reveal: false, log: true });

  test('a binding naming no command is reported, with its combo', () => {
    const dead = deadInvocations(
      [command('panel:swap')],
      [binding(['x'], 'panel:swap'), binding(['g', 'z'], 'panel:gone')],
    );
    expect(dead).toEqual([{ keys: 'g z', invocation: 'panel:gone' }]);
  });

  test('a pre-filled argument is not part of the name', () => {
    // `panel:set type file` names `panel:set`; reading the whole invocation as
    // a name would report every parameterized binding as dead.
    const dead = deadInvocations([command('panel:set')], [binding(['s', 'f'], 'panel:set type file')]);
    expect(dead).toEqual([]);
  });

  test('several combos on one dead command are each reported', () => {
    const dead = deadInvocations(
      [],
      [binding(['a'], 'nope:gone'), binding(['b'], 'nope:gone')],
    );
    expect(dead.map((d) => d.keys)).toEqual(['a', 'b']);
  });

  test('a shell command is not a name', () => {
    expect(deadInvocations([], [binding(['x'], '!ls -la')])).toEqual([]);
  });

  test('nothing to report is an empty list', () => {
    expect(deadInvocations([command('a:b')], [binding(['x'], 'a:b')])).toEqual([]);
  });
});

describe('listedCommands', () => {
  const binding = (keys: string[], invocation: string) => ({
    keys,
    invocation,
    when: null,
    text_input: false,
  });
  const command = (name: string, label = name) => ({
    name,
    label,
    owner: 'metarecord-detail',
    reveal: false,
    log: true,
  });

  const commands = [command('metarecord:bulk', 'Bulk edit'), command('metarecord-list:next')];
  const keytable = [
    binding(['m', 'b'], 'metarecord:bulk'),
    binding(['m', 'm', 'd'], 'metarecord:bulk delete'),
    binding(['m', 'm', 's'], 'metarecord:bulk set'),
    binding(['down'], 'metarecord-list:next'),
    binding(['j'], 'metarecord-list:next'),
  ];

  test('every registered command is listed', () => {
    const names = listedCommands(commands, keytable).map((c) => c.name);
    expect(names).toContain('metarecord:bulk');
    expect(names).toContain('metarecord-list:next');
  });

  test('a bound parameterized invocation earns its own entry', () => {
    const names = listedCommands(commands, keytable).map((c) => c.name);
    expect(names).toContain('metarecord:bulk delete');
    expect(names).toContain('metarecord:bulk set');
  });

  test('an unbound parameter combination is not listed', () => {
    // `unset` is a valid operation of the generic command, but no key runs it:
    // it stays discoverable through the operation prompt, not the listing.
    const names = listedCommands(commands, keytable).map((c) => c.name);
    expect(names).not.toContain('metarecord:bulk unset');
  });

  test('the bare entry shows only the combos bound to it exactly', () => {
    // Without this the bare command would collect every parameterized combo —
    // `panel:set type` used to print 14 of them on one line, with nothing
    // saying which combo went with which panel type.
    const listed = listedCommands(commands, keytable);
    expect(listed.find((c) => c.name === 'metarecord:bulk')?.shortcuts).toEqual(['m b']);
    expect(listed.find((c) => c.name === 'metarecord:bulk delete')?.shortcuts).toEqual(['m m d']);
  });

  test('several combos on one invocation make one entry', () => {
    const listed = listedCommands(commands, keytable);
    const next = listed.filter((c) => c.name === 'metarecord-list:next');
    expect(next).toHaveLength(1);
    expect(next[0].shortcuts).toEqual(['down', 'j']);
  });

  test('an expanded entry inherits the base command label and owner', () => {
    const listed = listedCommands(commands, keytable);
    const entry = listed.find((c) => c.name === 'metarecord:bulk delete');
    expect(entry?.label).toBe('Bulk edit');
    expect(entry?.owner).toBe('metarecord-detail');
  });

  test('a binding on an unregistered command is ignored', () => {
    const listed = listedCommands(commands, [binding(['z'], 'nope:gone param')]);
    expect(listed.map((c) => c.name)).not.toContain('nope:gone param');
  });
});

describe('shortcutsFor', () => {
  const binding = (keys: string[], invocation: string) => ({
    keys,
    invocation,
    when: null,
    text_input: false,
  });
  const table = [
    binding(['alt+t'], 'workspace:new'),
    binding(['ctrl+g'], 'metarecord-list:set mode grid'),
    binding(['down'], 'metarecord-list:next'),
    binding(['j'], 'metarecord-list:next'),
    binding(['g', 'g'], 'metarecord-list:goto-top'),
  ];

  test('exact invocation match', () => {
    expect(shortcutsFor(table, 'workspace:new')).toEqual(['alt+t']);
  });

  test('parameterized invocations count for the bare command', () => {
    expect(shortcutsFor(table, 'metarecord-list:set mode')).toEqual(['ctrl+g']);
  });

  test('several bindings are all listed', () => {
    expect(shortcutsFor(table, 'metarecord-list:next')).toEqual(['down', 'j']);
  });

  test('sequences are space-joined', () => {
    expect(shortcutsFor(table, 'metarecord-list:goto-top')).toEqual(['g g']);
  });

  test('no binding yields an empty list, not a partial-name match', () => {
    expect(shortcutsFor(table, 'workspace:close')).toEqual([]);
    expect(shortcutsFor(table, 'metarecord-list:go')).toEqual([]);
  });
});

describe('shouldLogCommand', () => {
  const commands = [
    { name: 'reconcile:run', log: true },
    { name: 'editing:confirm', log: false },
    { name: 'workspace:goto', log: true },
  ];

  test('logs a command whose definition opts in', () => {
    expect(shouldLogCommand(commands, 'reconcile:run')).toBe(true);
  });

  test('does not log a command that opts out', () => {
    expect(shouldLogCommand(commands, 'editing:confirm')).toBe(false);
  });

  test('unknown commands default to logging', () => {
    expect(shouldLogCommand(commands, 'p:never-registered')).toBe(true);
  });
});

describe('filterCommands', () => {
  const all = [
    { name: 'panel:unsplit', label: 'Hide the non-focused panel slot' },
    { name: 'panel:split', label: 'Show the second panel slot' },
    { name: 'quit', label: 'Exit the GUI' },
    { name: 'message:clear', label: 'Clear the workspace message log' },
  ];

  test('prefix matches come first, then substring matches', () => {
    const names = filterCommands(all, 'pa').map((c) => c.name);
    expect(names).toEqual(['panel:split', 'panel:unsplit']);

    // Substring matches are alphabetical among themselves ("sp" is a
    // substring of both, a prefix of neither).
    const sp = filterCommands(all, 'sp').map((c) => c.name);
    expect(sp).toEqual(['panel:split', 'panel:unsplit']);
  });

  test('empty filter lists everything', () => {
    expect(filterCommands(all, '').length).toBe(4);
  });

  test('no match yields an empty list', () => {
    expect(filterCommands(all, 'zzz')).toEqual([]);
  });

  test('space-separated terms match fuzzily, in order ("con def" ≈ .*con.*def.*)', () => {
    const names = filterCommands(all, 'pan spl').map((c) => c.name);
    expect(names).toEqual(['panel:split', 'panel:unsplit']);
    // Terms must appear in order, without overlapping.
    expect(filterCommands(all, 'spl pan')).toEqual([]);
    expect(filterCommands(all, 'mes cle').map((c) => c.name)).toEqual(['message:clear']);
  });

  test('matching is case-insensitive', () => {
    const names = filterCommands(all, 'PAN').map((c) => c.name);
    expect(names).toEqual(['panel:split', 'panel:unsplit']);
  });
});

describe('filterCompletions', () => {
  const tags = ['rock', 'jazz', 'jazz/bebop', 'classical'];

  test('prefix matches come first, then substring matches', () => {
    expect(filterCompletions(tags, 'jazz')).toEqual(['jazz', 'jazz/bebop']);
    expect(filterCompletions(tags, 'bop')).toEqual(['jazz/bebop']);
  });

  test('empty draft lists every completion, sorted', () => {
    expect(filterCompletions(tags, '')).toEqual(['classical', 'jazz', 'jazz/bebop', 'rock']);
  });

  test('no match yields an empty list', () => {
    expect(filterCompletions(tags, 'zzz')).toEqual([]);
  });

  test('space-separated terms match fuzzily, in order', () => {
    expect(filterCompletions(tags, 'ja be')).toEqual(['jazz/bebop']);
    expect(filterCompletions(tags, 'be ja')).toEqual([]);
  });

  test('caps the result so a huge completion set stays cheap to render', () => {
    const many = Array.from({ length: 5000 }, (_, i) => `dir/${String(i).padStart(4, '0')}`);
    expect(filterCompletions(many, '').length).toBe(200);
    // The cap keeps the best (sorted-first) matches, not an arbitrary slice.
    expect(filterCompletions(many, '')[0]).toBe('dir/0000');
    // An explicit smaller limit is honoured; a small set is unaffected.
    expect(filterCompletions(many, '', 10).length).toBe(10);
    expect(filterCompletions(tags, '', 200)).toEqual(['classical', 'jazz', 'jazz/bebop', 'rock']);
  });
});

describe('argSpecs registry', () => {
  afterEach(() => clearArgSpecs());

  const spec = (name: string): ArgSpec => ({ name, prompt: () => `${name}?` });

  test('registered args are retrievable by command name', () => {
    const args = [spec('field'), spec('value')];
    registerArgs('metarecord:edit-field-value', args);
    expect(argSpecFor('metarecord:edit-field-value')).toEqual(args);
  });

  test('unknown command has no arg spec', () => {
    expect(argSpecFor('never:registered')).toBeUndefined();
  });

  test('re-registration replaces the previous spec (panel reload)', () => {
    registerArgs('p:cmd', [spec('a')]);
    registerArgs('p:cmd', [spec('b'), spec('c')]);
    expect(argSpecFor('p:cmd')?.map((a) => a.name)).toEqual(['b', 'c']);
  });

  test('registering an empty arg list clears the spec', () => {
    registerArgs('p:cmd', [spec('a')]);
    registerArgs('p:cmd', []);
    expect(argSpecFor('p:cmd')).toBeUndefined();
  });

  // A panel command is registered once per *instance* — one per workspace ×
  // panel type — and every instance registers the same name. The arg spec
  // carries live closures reading that instance's state, so a name-keyed
  // global registry hands the focused workspace the completions of whichever
  // instance mounted last: the panel handler runs on your workspace while the
  // minibuffer offers another one's list. The resolver is asked first.
  test('the panel resolver wins over the global registry', () => {
    const mine = [spec('mine')];
    registerArgs('p:cmd', [spec('someone-else')]);
    setPanelArgs({
      prepare: async () => {},
      resolve: (name) => (name === 'p:cmd' ? mine : undefined),
    });
    expect(argSpecFor('p:cmd')).toBe(mine);
    setPanelArgs(null);
    expect(argSpecFor('p:cmd')?.map((a) => a.name)).toEqual(['someone-else']);
  });

  test('a command the panel resolver does not know falls back to the global registry', () => {
    // The mirrored spec: the right shape with another instance's closures,
    // which is all the synchronous "does it prompt?" probe needs.
    const args = [spec('target')];
    registerArgs('recent-ish', args);
    setPanelArgs({ prepare: async () => {}, resolve: () => undefined });
    expect(argSpecFor('recent-ish')).toBe(args);
    setPanelArgs(null);
  });

  test('dispatch mounts the owning panel instance before reading its spec', async () => {
    // Order matters: a workspace that has never shown the owning panel has no
    // instance, so reading the spec first would collect no arguments at all.
    const calls: string[] = [];
    setPanelArgs({
      prepare: async (name) => void calls.push(`prepare:${name}`),
      resolve: (name) => {
        calls.push(`resolve:${name}`);
        return undefined;
      },
    });
    await dispatch('p:panel-cmd');
    expect(calls).toEqual(['prepare:p:panel-cmd', 'resolve:p:panel-cmd']);
    setPanelArgs(null);
  });
});

describe('promptsForInput', () => {
  // The autocomplete marks a command with a trailing "…" when invoking it
  // reopens the minibuffer to collect input (doc "The command input"): the same
  // ellipsis convention as menu items that open a dialog. The signal is the
  // interactive-argument mechanism — a registered ArgSpec, which every command
  // that takes arguments declares, builtins included.
  afterEach(() => clearArgSpecs());

  test('a command with a registered arg spec prompts', () => {
    registerArgs('p:pick', [{ name: 'x', prompt: () => 'x?' }]);
    expect(promptsForInput('p:pick')).toBe(true);
  });

  test('a plain action command does not prompt', () => {
    expect(promptsForInput('panel:split')).toBe(false);
    expect(promptsForInput('never:registered')).toBe(false);
  });

  test('a builtin prompts through its declared arguments, like any other command', () => {
    // `workspace:rename` used to be the exception — a builtin that reopened
    // the input by hand without declaring anything. It declares its name like
    // every other command now, so one mechanism drives the marker.
    registerArgs('workspace:rename', [{ name: 'name', prompt: () => 'Rename to:' }]);
    expect(promptsForInput('workspace:rename')).toBe(true);
    expect(promptsForInput('workspace:rename new name')).toBe(false);
  });
});

describe('collectArgs', () => {
  // A prompt driver that returns scripted answers in order, recording every
  // request it was shown. A `null` in the script models the user pressing
  // Escape.
  function scriptedPrompt(answers: (string | null)[]) {
    const requests: ArgPromptRequest[] = [];
    let i = 0;
    const fn = vi.fn(async (request: ArgPromptRequest) => {
      requests.push(request);
      return answers[i++] ?? null;
    });
    return { fn, requests };
  }

  const value = (): ArgSpec => ({
    name: 'value',
    prompt: (prior) => `New value for ${prior[0]}?`,
    initial: (prior) => `current-${prior[0]}`,
    complete: (_partial, prior) => [`${prior[0]}/a`, `${prior[0]}/b`],
  });
  const field = (): ArgSpec => ({
    name: 'field',
    prompt: () => 'Which field?',
    complete: () => ['tag', 'rating'],
  });

  test('all arguments supplied inline: no prompting', async () => {
    const { fn } = scriptedPrompt([]);
    const result = await collectArgs([field(), value()], ['tag', 'jazz'], fn);
    expect(result).toEqual(['tag', 'jazz']);
    expect(fn).not.toHaveBeenCalled();
  });

  test('a missing trailing argument is prompted with resolved prompt/initial/completions', async () => {
    const { fn, requests } = scriptedPrompt(['musique/jazz']);
    const result = await collectArgs([field(), value()], ['tag'], fn);
    expect(result).toEqual(['tag', 'musique/jazz']);
    expect(requests.map((r) => [r.argName, r.prompt, r.initial])).toEqual([
      ['value', 'New value for tag?', 'current-tag'],
    ]);
    expect(await requests[0].completions).toEqual(['tag/a', 'tag/b']);
  });

  test('all arguments missing are prompted in order, prior args accumulating', async () => {
    const { fn, requests } = scriptedPrompt(['tag', 'musique/jazz']);
    const result = await collectArgs([field(), value()], [], fn);
    expect(result).toEqual(['tag', 'musique/jazz']);
    // The second request's prompt/initial/completions saw the first answer.
    expect(requests[0].prompt).toBe('Which field?');
    expect([requests[1].argName, requests[1].prompt, requests[1].initial]).toEqual([
      'value',
      'New value for tag?',
      'current-tag',
    ]);
    expect(await requests[1].completions).toEqual(['tag/a', 'tag/b']);
  });

  // A generic command carries one spec per argument any of its operations can
  // take, and `when` drops the ones the chosen operation has no use for:
  // `metarecord:bulk delete` names no field, `metarecord:bulk unset` no value.
  const operation = (): ArgSpec => ({
    name: 'operation',
    prompt: () => 'Operation?',
    complete: () => ['set', 'unset', 'delete'],
  });
  const fieldUnlessDelete = (): ArgSpec => ({
    ...field(),
    when: (prior) => prior[0] !== 'delete',
  });
  const valueWhenSet = (): ArgSpec => ({
    ...value(),
    when: (prior) => prior[0] === 'set',
  });
  const bulkSpecs = () => [operation(), fieldUnlessDelete(), valueWhenSet()];

  test('a spec whose `when` is false is never prompted', async () => {
    const { fn, requests } = scriptedPrompt([]);
    const result = await collectArgs(bulkSpecs(), ['delete'], fn);
    expect(result).toEqual(['delete']);
    expect(requests).toEqual([]);
  });

  test('`when` skips an argument mid-way and still prompts the later ones', async () => {
    const { fn, requests } = scriptedPrompt(['tag']);
    const result = await collectArgs(bulkSpecs(), ['unset'], fn);
    expect(result).toEqual(['unset', 'tag']);
    expect(requests.map((r) => r.argName)).toEqual(['field']);
  });

  test('inline arguments stay aligned with the specs that survive `when`', async () => {
    // 'delete' skips the field spec, so the tokens after it must not be read
    // as a field — there is no second argument to fill.
    const { fn } = scriptedPrompt([]);
    expect(await collectArgs(bulkSpecs(), ['set', 'tag', 'jazz'], fn)).toEqual([
      'set',
      'tag',
      'jazz',
    ]);
    expect(await collectArgs(bulkSpecs(), ['unset', 'tag'], fn)).toEqual(['unset', 'tag']);
    expect(fn).not.toHaveBeenCalled();
  });

  // An optional trailing argument: supplied inline it is used, omitted it is
  // simply absent — the command falls back to its default rather than stopping
  // to ask. `metarecord-list:apply <zone> [stay]`, `log:revert [what]`.
  const stay = (): ArgSpec => ({
    name: 'stay',
    optional: true,
    prompt: () => 'Stay?',
  });

  test('an optional argument supplied inline is collected', async () => {
    const { fn } = scriptedPrompt([]);
    expect(await collectArgs([field(), stay()], ['tag', 'stay'], fn)).toEqual(['tag', 'stay']);
    expect(fn).not.toHaveBeenCalled();
  });

  test('an optional argument left out is skipped, never prompted', async () => {
    const { fn } = scriptedPrompt([]);
    expect(await collectArgs([field(), stay()], ['tag'], fn)).toEqual(['tag']);
    expect(fn).not.toHaveBeenCalled();
  });

  test('a required argument before an optional one is still prompted', async () => {
    const { fn, requests } = scriptedPrompt(['tag']);
    expect(await collectArgs([field(), stay()], [], fn)).toEqual(['tag']);
    expect(requests.map((r) => r.argName)).toEqual(['field']);
  });

  test('Escape (null) abandons the whole invocation and stops prompting', async () => {
    const { fn } = scriptedPrompt([null]);
    const result = await collectArgs([field(), value()], [], fn);
    expect(result).toBeNull();
    // Only the first argument was ever prompted.
    expect(fn).toHaveBeenCalledTimes(1);
  });

  test('the last declared argument absorbs extra inline tokens (joined by space)', async () => {
    const { fn } = scriptedPrompt([]);
    const result = await collectArgs([field(), value()], ['tag', 'musique', 'jazz'], fn);
    expect(result).toEqual(['tag', 'musique jazz']);
    expect(fn).not.toHaveBeenCalled();
  });

  // Building a candidate list can be expensive (every path of a TreeRef forest
  // is one daemon round-trip plus tens of thousands of strings). Awaiting it
  // before opening the input froze the GUI for seconds on a large repository —
  // the prompt must appear at once and take the candidates when they land.
  test('a slow completion source does not delay the prompt', async () => {
    let release: (list: string[]) => void = () => {};
    const slow: ArgSpec = {
      name: 'value',
      prompt: () => 'Value?',
      complete: () => new Promise<string[]>((resolve) => (release = resolve)),
    };
    const { fn, requests } = scriptedPrompt(['typed-by-hand']);

    const result = await collectArgs([slow], [], fn);

    // Prompted and answered while the candidates were still being built.
    expect(result).toEqual(['typed-by-hand']);
    expect(requests).toHaveLength(1);
    release(['a', 'b']);
    await expect(Promise.resolve(requests[0].completions)).resolves.toEqual(['a', 'b']);
  });

  test('absent initial/complete yield empty initial and no completions', async () => {
    const bare: ArgSpec = { name: 'x', prompt: () => 'X?' };
    const { fn, requests } = scriptedPrompt(['v']);
    await collectArgs([bare], [], fn);
    expect(requests[0]).toEqual({ argName: 'x', prompt: 'X?', initial: '', completions: [] });
  });
});

describe('resolveSubmission', () => {
  const sugg = [{ name: 'panel:swap' }, { name: 'panel:split' }];

  test('runs the highlighted suggestion when the list is non-empty', () => {
    expect(resolveSubmission('pan', sugg, 0)).toBe('panel:swap');
    expect(resolveSubmission('pan', sugg, 1)).toBe('panel:split');
  });

  test('clamps an out-of-range selection', () => {
    expect(resolveSubmission('pan', sugg, 9)).toBe('panel:split');
    expect(resolveSubmission('pan', sugg, -1)).toBe('panel:swap');
  });

  test('a typed invocation of a known command wins over the list', () => {
    // `panel:set type` is an incomplete invocation: it asks for the type. It
    // must not run the first bound variant the list happens to show.
    const known = ['panel:set', 'help', 'help:cursor'];
    const variants = [{ name: 'panel:set type duplicates' }, { name: 'panel:set type file' }];
    expect(resolveSubmission('panel:set type', variants, 0, known)).toBe('panel:set type');
    // An abbreviation names no command: the highlighted row runs.
    expect(resolveSubmission('pan ty', variants, 0, known)).toBe('panel:set type duplicates');
    // A highlight the user moved is a choice, and wins.
    expect(resolveSubmission('panel:set type', variants, 1, known)).toBe('panel:set type file');
    const helps = [{ name: 'help' }, { name: 'help:cursor' }];
    expect(resolveSubmission('help', helps, 1, known)).toBe('help:cursor');
  });

  test('falls back to the typed text when there is no suggestion', () => {
    expect(resolveSubmission('panel:set type file', [], 0)).toBe('panel:set type file');
    expect(resolveSubmission('!ls', [], 0)).toBe('!ls');
  });
});

describe('resolvePromptValue', () => {
  const sugg = [{ name: 'jazz' }, { name: 'jazz/bebop' }];

  test('accepts the highlighted completion (plain Enter)', () => {
    expect(resolvePromptValue('ja', sugg, 0, false)).toBe('jazz');
    expect(resolvePromptValue('ja', sugg, 1, false)).toBe('jazz/bebop');
  });

  test('clamps an out-of-range selection', () => {
    expect(resolvePromptValue('ja', sugg, 9, false)).toBe('jazz/bebop');
  });

  test('raw submits the typed text verbatim even with a highlight (Ctrl-Enter)', () => {
    expect(resolvePromptValue('jazz', sugg, 0, true)).toBe('jazz');
    // A brand-new value that ordered-substring-matches an existing completion:
    // Ctrl-Enter creates it instead of picking the suggestion.
    expect(resolvePromptValue('ja', sugg, 0, true)).toBe('ja');
  });

  test('a deselected list (index < 0) submits the typed text', () => {
    expect(resolvePromptValue('ja', sugg, -1, false)).toBe('ja');
  });

  test('an empty completion list submits the typed text', () => {
    expect(resolvePromptValue('newvalue', [], 0, false)).toBe('newvalue');
  });

  // The candidate is a couple (doc "Completion views"): the label is what
  // is shown and matched whole, the value is what the command receives.
  const named = [
    { name: 'musique (uuid-1)', value: 'uuid-1' },
    { name: 'musique (uuid-2)', value: 'uuid-2' },
  ];

  test('the highlighted candidate submits its *value*', () => {
    expect(resolvePromptValue('mus', named, 1, false)).toBe('uuid-2');
  });

  test('typed text that spells a label whole names its value', () => {
    // What is seen is what is typed; the label is a name, not the answer.
    expect(resolvePromptValue('musique (uuid-1)', named, -1, false, named)).toBe('uuid-1');
  });

  test('typed text nothing names stays itself (an uuid, a new value)', () => {
    expect(resolvePromptValue('uuid-3', named, -1, true, named)).toBe('uuid-3');
    expect(resolvePromptValue('musique (uuid-9)', named, -1, true, named)).toBe('musique (uuid-9)');
  });

  test('a typed label resolves even under Ctrl-Enter — what is typed is a name', () => {
    expect(resolvePromptValue('musique (uuid-2)', named, 0, true, named)).toBe('uuid-2');
  });
});

describe('completion views (doc "Completion views")', () => {
  function scriptedPrompt(answers: (string | null)[]) {
    const requests: ArgPromptRequest[] = [];
    let i = 0;
    const fn = vi.fn(async (request: ArgPromptRequest) => {
      requests.push(request);
      return answers[i++] ?? null;
    });
    return { fn, requests };
  }

  test('one builder is one view; an array of them is what the cycle walks', async () => {
    const views: ArgSpec = {
      name: 'value',
      prompt: () => 'Value?',
      complete: [
        { title: 'path · name', items: () => [{ label: 'musique', value: 'uuid-1' }] },
        { title: 'name', items: (partial) => ({ items: [partial], more: true }) },
      ],
    };
    const { fn, requests } = scriptedPrompt(['uuid-1']);
    expect(await collectArgs([views], [], fn)).toEqual(['uuid-1']);
    const source = await requests[0].source!;
    expect(source.views.map((view) => view.title)).toEqual(['path · name', 'name']);
    // The eager first page is the first view's own page.
    expect(await requests[0].completions).toEqual([{ label: 'musique', value: 'uuid-1' }]);
    // The other view narrows on the typed text and reports it is not the whole
    // set (`more`) — so narrowing means asking again, not filtering.
    expect(await source.views[1].items('mu')).toEqual({ items: ['mu'], more: true });
  });

  test('a bound view is called with the typed text and the arguments collected so far', async () => {
    const seen: string[][] = [];
    const spec: ArgSpec = {
      name: 'value',
      prompt: () => 'Value?',
      complete: [
        {
          items: (partial, prior) => {
            seen.push([partial, ...prior]);
            return [];
          },
        },
      ],
    };
    const { fn, requests } = scriptedPrompt(['x']);
    await collectArgs([{ name: 'field', prompt: () => 'F?' }, spec], ['tag'], fn);
    expect(await (await requests[0].source!).views[0].items('mu')).toEqual({ items: [] });
    expect(seen).toEqual([
      ['', 'tag'], // the eager first page
      ['mu', 'tag'], // the narrowing query — same `prior`, bound at collection
    ]);
  });

  test('the eager page and the driver’s first load are one call (memoized)', async () => {
    let calls = 0;
    const spec: ArgSpec = {
      name: 'value',
      prompt: () => 'Value?',
      complete: [{ items: () => (calls += 1, [{ label: 'a', value: 'a' }]) }],
    };
    const { fn, requests } = scriptedPrompt(['a']);
    await collectArgs([spec], [], fn);
    expect(await requests[0].completions).toEqual([{ label: 'a', value: 'a' }]);
    expect(await (await requests[0].source!).views[0].items('')).toEqual({
      items: [{ label: 'a', value: 'a' }],
    });
    expect(calls).toBe(1);
  });

  test('filterCompletionItems ranks on the label and carries the value along', () => {
    const items = [
      { label: 'musique', value: 'uuid-1' },
      { label: 'musique (jazz)', value: 'uuid-2' },
    ];
    expect(filterCompletionItems(items, 'mus')).toEqual(items);
    expect(filterCompletionItems(items, 'jazz')).toEqual([items[1]]);
  });
});
