// The list's text zones, driven by `focus` and `insert`, each taking the zone as
// its first argument rather than one command per (verb, zone) pair, and by
// `apply-query`, which takes no zone: applying any of them applies them all
// (doc "metarecord-list panel"). What they need from the panel arrives
// through `ctx`:
//   root       -> the panel's Shadow root (whose focus `apply-query` gives up)
//   zones      -> the zone table: a new zone costs one entry, not three commands
//   clearAll(mf) -> `clear-all`: reset every search field and re-run
//   applyQuery(mf) -> `apply-query`: commit every zone and run the query once
// Each zone function takes first the API of the command's run (doc "Command
// transactions").

/**
 * @typedef {object} Zone
 * @property {HTMLInputElement} input       the element focus lands on
 * @property {(mf: MetafolderApi) => unknown} [clear]  empty it (focus follows)
 *
 * @param {{
 *   commands: MetafolderApi['commands'],
 *   root: ShadowRoot,
 *   zones: Record<string, Zone>,
 *   clearAll: (mf: MetafolderApi) => unknown,
 *   applyQuery: (mf: MetafolderApi) => unknown,
 * }} ctx
 */
export function registerZoneCommands({ commands, root, zones: ZONES, clearAll, applyQuery }) {
  const ZONE_NAMES = Object.keys(ZONES);

  /** @param {string} name */
  function zone(name) {
    const found = ZONES[name];
    if (!found) throw new Error(`unknown zone: "${name ?? ''}" (expected ${ZONE_NAMES.join(' / ')})`);
    return found;
  }

  /** @param {string} verb */
  const zoneArg = (verb) => ({
    name: 'zone',
    prompt: () => `Which zone to ${verb}? (${ZONE_NAMES.join(' / ')})`,
    complete: () => ZONE_NAMES,
  });

  void commands.register('metarecord-list:focus', {
    label: `Metarecord list: focus a zone (${ZONE_NAMES.join(' / ')})`,
    args: [zoneArg('focus'), { name: 'clear', optional: true, prompt: () => 'Empty it first? (clear)' }],
    handler: async (mf, name, modifier) => {
      const z = zone(name);
      if (modifier !== undefined && modifier !== 'clear') {
        throw new Error(`unknown modifier: "${modifier}" (expected clear)`);
      }
      if (modifier === 'clear') await z.clear?.(mf);
      z.input.focus();
    },
  });

  // Resets the three search fields at once and re-runs; an empty query matches
  // everything, so it goes back to the whole repository. No zone takes the focus.
  void commands.register('metarecord-list:clear-all', {
    label: 'Metarecord list: empty the search fields and list everything',
    handler: (mf) => clearAll(mf),
  });

  // The zones are one query — finder AND simplified AND normal DSL, shown in
  // the chosen columns —, so there is one apply for all of them, not one per zone.
  void commands.register('metarecord-list:apply-query', {
    label: 'Metarecord list: apply the query and the columns',
    args: [
      // Enter in a zone normally leaves it, so the panel accelerators resume
      // without a separate Escape; `stay` is the keep-the-focus variant
      // (Shift+Enter). One verb with a modifier, not two verbs.
      { name: 'stay', optional: true, prompt: () => 'Stay in the zone? (stay)' },
    ],
    handler: async (mf, stay) => {
      if (stay !== undefined && stay !== 'stay') {
        throw new Error(`unknown modifier: "${stay}" (expected stay)`);
      }
      await applyQuery(mf);
      // Only ever gives up a focus a zone holds: applying from a button or a
      // key must not pull the caret out of another field of the panel.
      const focused = Object.values(ZONES).find((z) => root.activeElement === z.input);
      if (stay !== 'stay') focused?.input.blur();
    },
  });

  void commands.register('metarecord-list:insert', {
    label: `Metarecord list: insert text into a zone (${ZONE_NAMES.join(' / ')})`,
    args: [
      zoneArg('insert into'),
      { name: 'text', prompt: (p) => `Text to insert into the ${p[0]} zone?` },
      // `stay` is the modifier `apply-query` takes and means the same: the focus
      // stays where it is. A plain insert lands in the zone, caret after the
      // text, ready for the follow-up edit; a caller that writes and then runs
      // the query (`tag:query`) must not yank the focus out of whatever
      // the user is doing.
      { name: 'stay', optional: true, prompt: () => 'Leave the focus where it is? (stay)' },
    ],
    handler: async (mf, name, text, modifier) => {
      // `text` is free text and is declared before `stay`, so a trailing run
      // of tokens lands in the modifier: `insert simplified #=jazz rock` must
      // splice the whole "#=jazz rock". Re-join it back into the text, and
      // read only the exact token `stay` as the modifier — quote the text to
      // splice a literal trailing "stay".
      const stay = modifier === 'stay';
      const value = stay || modifier === undefined ? text : `${text} ${modifier}`;
      const input = zone(name).input;
      const from = input.selectionStart ?? input.value.length;
      const to = input.selectionEnd ?? from;
      input.value = input.value.slice(0, from) + value + input.value.slice(to);
      const caret = from + value.length;
      input.setSelectionRange(caret, caret);
      if (!stay) input.focus();
      // Assigning `.value` fires nothing, so the debounced preview of the
      // simplified query's expansion would go stale (panel-shim/history.js
      // dispatches this for the same reason).
      input.dispatchEvent(new Event('input', { bubbles: true }));
      // Deliberately does not run: inserting prepares the query, `apply-query` runs it.
    },
  });
}
