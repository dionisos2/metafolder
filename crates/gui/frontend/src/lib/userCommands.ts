// Loading ~/.config/metafolder/gui/commands.js (spec-gui "User commands").
//
// The module is served by the GUI's own HTTP server, like a panel's, and
// imported into the shell realm. Unlike a panel it is NOT wrapped in an error
// boundary: a broken command file stops the GUI, the way a broken keybindings
// file does. Configuration is corrected before the GUI runs, not around.

import {
  clearUserCommands,
  dispatch,
  installUserCommands,
  setUserCommandReloader,
  validateUserCommands,
} from './commands';
import { invoke } from './ipc';
import { createUserCommandApi } from './panels/api';
import { focusedWs, refreshCommands, store } from './store.svelte';

/** Bumped on every reload so the WebView re-imports instead of serving its
 *  cached module — the same cache-bust PanelHost applies to panel modules. */
let generation = 0;

/**
 * Imports the user's command module and installs what it exports.
 *
 * Rejects — loudly, on purpose — when the file is missing, does not parse, or
 * exports something malformed. At boot the rejection reaches App.svelte's
 * failure banner and the UI never appears; on a reload it reaches the status
 * bar and the commands already installed stay as they were.
 */
export async function loadUserCommands(): Promise<string[]> {
  generation += 1;
  const base = `http://127.0.0.1:${store.guiPort}`;
  // Imported BEFORE anything is torn down: a reload that fails must leave the
  // commands already installed alone, or one bad edit would take them all away
  // until the file parses again.
  const module = (await import(
    /* @vite-ignore */ `${base}/__commands.js?v=${generation}`
  )) as { default?: unknown };

  // Validated before anything is torn down, for the same reason the import is:
  // a malformed entry must cost the reload, not the commands you already had.
  validateUserCommands(module.default);

  const previous = clearUserCommands();
  if (previous.length > 0) await invoke('forget_user_commands', { names: previous });

  const api = createUserCommandApi(
    {
      invoke,
      dispatch,
      // A user command has no panel, so it registers no panel handler and
      // contributes no context menu; the command set it changes is refreshed
      // by this loader, not per registration.
      registerHandler: () => {},
      onCommandsChanged: () => {},
      addDefaultMenuItems: () => {},
    },
    { guiServer: base, sessionToken: store.sessionToken, focusedWs },
  );
  const names = await installUserCommands(module.default, api, (name, label, log) =>
    invoke('register_user_command', { name, label, log }) as Promise<void>,
  );
  await refreshCommands();
  return names;
}

setUserCommandReloader(loadUserCommands);
