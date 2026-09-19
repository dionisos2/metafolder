// Your own GUI commands.
//
// This module is imported into the shell when the GUI starts. Its default
// export maps a command name to its definition — the key IS the name, so there
// is no `register` call and no boilerplate around it:
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
// A command here composes existing commands through `mf.invoke`. If something
// you want cannot be said that way, that is usually a base command missing
// rather than a reason to reach into the daemon from here.
//
// A syntax error in this file stops the GUI from starting, with the error on
// screen. `config:reload commands` re-reads it without a restart.

/** Every path of a TreeRef field in the active repository, sorted. */
async function treePaths(mf, field) {
  const repo = await mf.workspace.get('active_repo');
  if (!repo) return [];
  const body = await mf.daemon.call('POST', `/repos/${repo}/query/fields/resolve-tree`, {
    query: { type: 'is_present', field },
    field,
  });
  const paths = new Set();
  for (const list of Object.values(body ?? {})) for (const p of list) paths.add(p);
  return [...paths].sort();
}

export default {
  // Insert a tag filter into the simplified query zone, completing over the
  // tags that exist. `#=` is the simplified language's "this exact tag path".
  'user:tag-query': {
    label: 'Insert a tag filter in the simplified query',
    args: [
      {
        name: 'tag',
        prompt: () => 'Tag?',
        complete: (mf) => treePaths(mf, 'tag'),
      },
    ],
    run: (mf, tag) => mf.invoke(`metarecord-list:insert simplified #=${tag}`),
  },

  // Rate the selection without going through the bulk operation picker.
  'user:rate': {
    label: 'Rate the selected metarecords',
    args: [
      {
        name: 'rating',
        prompt: () => 'Rating? (1-5)',
        complete: () => ['1', '2', '3', '4', '5'],
      },
    ],
    run: (mf, rating) => mf.invoke(`metarecord:bulk set rating ${rating}`),
  },
};
