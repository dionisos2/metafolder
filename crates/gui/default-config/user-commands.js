// Your own GUI commands (doc "User commands").
//
// This module is imported into the shell when the GUI starts, after
// `commands.js` — the GUI's own commands in JavaScript, beside this file.
// Its default export maps a command name to its definition — the key IS the
// name, so there is no `register` call and no boilerplate around it:
//
//   'user:thing': {
//     label: 'What the command input shows beside the name',
//     args: [{ name: 'x', prompt: (mf) => 'X?', complete: (mf) => [...] }],
//     run: (mf, x) => ...,
//   }
//
// Every function is handed `mf` first: the same `metafolder` API panels get,
// scoped to the *focused workspace* (a user command has no panel of its own,
// so the panel-only parts are absent). `run` then receives the arguments that
// were collected, inline or through the minibuffer.
//
// A command here composes existing commands through `mf.invoke` — those of
// `commands.js` included. If something you want cannot be said that way, that
// is usually a base command missing rather than a reason to reach into the
// daemon from here. The helpers `commands.js` exports can be imported:
//
//   import { selectedMetarecord, tagPaths } from './commands.js';
//
// A name already taken — by the shell, a panel or `commands.js` — is left
// out, and the status bar says so. Use `user:` to keep yours apart.
//
// A syntax error in this file stops the GUI from starting, with the error on
// screen. `config:reload commands` re-reads it without a restart.

export default {
  // Rate the selection without going through the bulk operation picker.
  // The type is named (`int`): on a repository with no `rating` yet, the
  // bulk write would otherwise stop and ask for it.
  'user:rate': {
    label: 'Rate the selected metarecords',
    args: [
      {
        name: 'rating',
        prompt: () => 'Rating? (1-5)',
        complete: () => ['1', '2', '3', '4', '5'],
      },
    ],
    run: (/** @type {MetafolderApi} */ mf, /** @type {string} */ rating) =>
      mf.invoke(`metarecord:bulk selection set rating int ${rating}`),
  },
};
