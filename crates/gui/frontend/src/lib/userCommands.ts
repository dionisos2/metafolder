// Loading ~/.config/metafolder/gui/commands.js and user-commands.js (doc
// "User commands").
//
// The modules are served by the GUI's own HTTP server, like a panel's, and
// imported into the shell realm. Unlike a panel it is NOT wrapped in an error
// boundary: a broken command file stops the GUI, the way a broken keybindings
// file does. Configuration is corrected before the GUI runs, not around.

import {
  clearUserCommands,
  dispatch,
  flushTransaction,
  installUserCommands,
  mergeCommandModules,
  setUserCommandReloader,
  status,
} from './commands';
import { invoke } from './ipc';
import { createUserCommandApi } from './panels/api';
import { focusedContext, refreshCommands, store } from './store.svelte';
import type { ExecContext } from './types';

/** Bumped on every reload so the WebView re-imports instead of serving its
 *  cached module — the same cache-bust PanelHost applies to panel modules. */
let generation = 0;

/**
 * Imports the two command modules and installs what they export.
 *
 * Rejects — loudly, on purpose — when a file is missing, does not parse, or
 * exports something malformed. At boot the rejection reaches App.svelte's
 * failure banner and the UI never appears; on a reload it reaches the status
 * bar and the commands already installed stay as they were.
 */
export async function loadUserCommands(): Promise<string[]> {
  generation += 1;
  const base = `http://127.0.0.1:${store.guiPort}`;
  // One directory per generation: `user-commands.js` imports `./commands.js`,
  // which then resolves to the very module imported here — the same instance,
  // and after a reload the new one rather than the WebView's cached copy.
  const dir = `${base}/__commands/${generation}`;
  // Imported BEFORE anything is torn down: a reload that fails must leave the
  // commands already installed alone, or one bad edit would take them all away
  // until the file parses again.
  const shipped = (await import(/* @vite-ignore */ `${dir}/commands.js`)) as {
    default?: unknown;
  };
  const own = (await import(/* @vite-ignore */ `${dir}/user-commands.js`)) as {
    default?: unknown;
  };
  // The macros `macro:save` wrote (doc "Macros"): user commands too.
  const saved = (await import(/* @vite-ignore */ `${dir}/macros.js`)) as {
    default?: unknown;
  };

  // Validated before anything is torn down, for the same reason the import is:
  // a malformed entry must cost the reload, not the commands you already had.
  const { module, shadowed } = mergeCommandModules(shipped.default, own.default, saved.default);
  const fileOf = (name: string) =>
    Object.hasOwn(shipped.default as object, name)
      ? 'commands.js'
      : Object.hasOwn(own.default as object, name)
        ? 'user-commands.js'
        : 'macros.js';

  const previous = clearUserCommands();
  if (previous.length > 0) await invoke('forget_user_commands', { names: previous });

  const deps = {
    invoke,
    dispatch,
    flush: flushTransaction,
    // A user command has no panel, so it registers no panel handler, no
    // per-instance arg spec (`commands.register` is not even offered to it —
    // its arguments are declared in the exported definition and land in the
    // shell-wide registry) and contributes no context menu; the command set
    // it changes is refreshed by this loader, not per registration.
    registerHandler: () => {},
    registerArgs: () => {},
    onCommandsChanged: () => {},
    addDefaultMenuItems: () => {},
  };
  const server = {
    guiServer: base,
    sessionToken: store.sessionToken,
    panelSettings: store.panelSettings,
  };
  // The prompts and completions follow the focus; a run gets an API bound to
  // the context it runs in (doc "Commands").
  const api = createUserCommandApi(deps, { ...server, context: focusedContext });
  const apiFor = (context: ExecContext) =>
    createUserCommandApi(deps, { ...server, context: () => context });
  const refused: string[] = [];
  const names = await installUserCommands(
    module,
    api,
    (name, label, log) => invoke<boolean>('register_user_command', { name, label, log }),
    (name) => refused.push(name),
    apiFor,
  );
  await refreshCommands();
  // Said rather than swallowed: the entry is in the file and does nothing.
  if (shadowed.length > 0) {
    await status(
      `${shadowed.map(({ file, name }) => `${file}: ${name}`).join(', ')} — already ${
        shadowed.length > 1 ? 'commands' : 'a command'
      } of a file loaded before, left out; the first one stands`,
    );
  }
  if (refused.length > 0) {
    await status(
      refused
        .map((name) => `${fileOf(name)}: ${name}`)
        .join(', ') +
        ` — already ${refused.length > 1 ? 'commands' : 'a command'} of the GUI, left out; the existing one stands`,
    );
  }
  return names;
}

setUserCommandReloader(loadUserCommands);
