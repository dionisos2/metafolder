// The help pages name commands, not keys (doc "How the help panel finds a page"): every
// `data-mf-key` is filled in at display from the live keybinding table. A typo
// in one of those names would silently render "unbound" — the page would claim
// a shortcut does not exist. So pin them against the commands that actually
// exist: the Rust builtins, the ones the panels register, and the ones the
// shipped commands.js defines.

import { readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { describe, expect, test } from 'vitest';

const guiDir = resolve(process.cwd(), '..');
const pagesDir = join(guiDir, 'default-config/panel-types/help/pages');
const panelsDir = join(guiDir, 'default-config/panel-types');

/** Builtins registered by the Rust shell: every command-shaped string literal
 *  of lib.rs (a superset — extra names only weaken this check, never break
 *  it), plus the two builtins whose name carries no colon. */
function builtinCommands(): Set<string> {
  const text = readFileSync(join(guiDir, 'src/lib.rs'), 'utf8');
  const names = [...text.matchAll(/"([a-z][a-z0-9-]*:[a-z0-9-]+)"/g)].map((m) => m[1]);
  return new Set([...names, 'help', 'quit']);
}

/** Commands the shipped panels register at mount, however they register them. */
function panelCommands(): Set<string> {
  const names = new Set<string>();
  for (const file of readdirSync(panelsDir, { recursive: true, withFileTypes: true })) {
    if (!file.isFile() || !file.name.endsWith('.js')) continue;
    const text = readFileSync(join(file.parentPath, file.name), 'utf8');
    for (const m of text.matchAll(/(?:commands\.register|registerFind\(metafolder,)\s*\(?\s*'([^']+)'/g)) {
      names.add(m[1]);
    }
  }
  return names;
}

/** Commands the shipped commands.js defines (spec-gui "User commands"): the
 *  name is the entry's key in the default export (`'name': {`). The scan is a
 *  superset at worst — extra names only weaken the check. */
function userCommands(): Set<string> {
  const text = readFileSync(join(guiDir, 'default-config/commands.js'), 'utf8');
  return new Set([...text.matchAll(/'([^']*)':\s*\{/g)].map((m) => m[1]));
}

/** Every command named by a `data-mf-key`, with the page it appears on. */
function taggedCommands(): { page: string; command: string }[] {
  const out: { page: string; command: string }[] = [];
  for (const file of readdirSync(pagesDir)) {
    if (!file.endsWith('.html')) continue;
    const text = readFileSync(join(pagesDir, file), 'utf8');
    for (const m of text.matchAll(/data-mf-key="([^"]+)"/g)) {
      for (const entry of m[1].split(',')) {
        // An invocation may carry arguments (`panel:set type treeref`); the
        // command is its first word.
        const command = entry.trim().split(/\s+/)[0];
        if (command) out.push({ page: file, command });
      }
    }
  }
  return out;
}

/** Every invocation the shipped keybindings.toml binds. */
function shippedInvocations(): string[] {
  const text = readFileSync(join(guiDir, 'default-config/keybindings.toml'), 'utf8');
  return [...text.matchAll(/command = "([^"]+)"/g)].map((m) => m[1]);
}

/** Every hint as it is written (the whole invocation, arguments included). */
function taggedInvocations(): { page: string; invocation: string }[] {
  const out: { page: string; invocation: string }[] = [];
  for (const file of readdirSync(pagesDir)) {
    if (!file.endsWith('.html')) continue;
    const text = readFileSync(join(pagesDir, file), 'utf8');
    for (const m of text.matchAll(/data-mf-key="([^"]+)"/g)) {
      for (const entry of m[1].split(',')) {
        const invocation = entry.trim();
        if (invocation) out.push({ page: file, invocation });
      }
    }
  }
  return out;
}

describe('help page key hints', () => {
  test('the pages do tag their shortcuts', () => {
    const tagged = taggedCommands();
    expect(tagged.length).toBeGreaterThan(50);
    expect(new Set(tagged.map((t) => t.page)).size).toBeGreaterThan(5);
  });

  test('no shipped page advertises a shortcut the shipped bindings do not give', () => {
    // A hint whose command is unbound renders "unbound" — honest, but out of the
    // box every hint should name a key. (`keysFor` matching rule: a bare command
    // matches any invocation of it, a fuller one only itself.)
    const invocations = shippedInvocations();
    const unbound = taggedInvocations().filter(
      ({ invocation }) =>
        !invocations.some((i) => i === invocation || i.startsWith(`${invocation} `)),
    );
    expect(unbound).toEqual([]);
  });

  test('every command a page names exists', () => {
    const known = new Set([...builtinCommands(), ...panelCommands(), ...userCommands()]);
    const unknown = taggedCommands().filter((t) => !known.has(t.command));
    expect(unknown).toEqual([]);
  });
});
