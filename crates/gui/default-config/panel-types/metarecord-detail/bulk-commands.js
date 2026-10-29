// `metarecord:bulk`: the set-layer mirror of `metarecord:field`, over the
// checked selection or the query the list shows instead of the displayed
// metarecord (doc "Editing metarecords in the GUI"). What it needs from the
// detail panel arrives through `ctx`:
//   repo()          -> the repository to act in (the shown record's, else the active one)
//   shownRepo()     -> the shown record's repository, or null
//   force(...names) -> the `force` a write to those fields carries (throws when refused)
//   dirty(mf)       -> tell the other panels the metarecords changed, through
//                      the API of the command's run (doc "Command transactions")

import { MATCH_ALL } from '/__value-widget.js';
import { settledType, splitTypeValue } from './field-args.js';
import { CONCRETE_TYPES } from './values.js';

/**
 * @param {{
 *   metafolder: MetafolderApi,
 *   vocab: ReturnType<typeof import('./values.js').createValues>,
 *   catalog: ReturnType<typeof import('./values.js').createFieldCatalog>,
 *   repo: () => Promise<string|null>,
 *   shownRepo: () => string|null,
 *   force: (...names: string[]) => {force?: true},
 *   dirty: (mf: MetafolderApi) => Promise<unknown>,
 *   statusMessageMs: number,
 * }} ctx
 */
export function registerBulkCommand({
  metafolder,
  vocab,
  catalog,
  repo: activeRepo,
  shownRepo,
  force: forceFor,
  dirty,
  statusMessageMs,
}) {
  const { daemon, workspace, commands, statusBar } = metafolder;

  /** Makes `repo`'s field catalogue readable synchronously (the type
   *  argument's `when` cannot await). @param {string|null} r */
  async function warmCatalog(r) {
    if (r) await catalog.read(r);
  }

  // ── Bulk field-editing commands (target argument: selection / query) ────
  // These take an explicit target as their first argument: `selection` acts on
  // the checkbox selection (`selected_metarecords`), `query` on the query the
  // list actually shows — metarecord-list publishes its *effective* query IR
  // (base query AND the live finder clause) as `metarecord-list:effective-query`.
  // If that var is absent (the list has not run), fall back to parsing the base
  // query text. There is no implicit choice between the two: a missing
  // selection used to fall through to the query, which silently re-targeted a
  // bulk write the user thought was scoped to their checks.

  /** @typedef {{repo: string, query: unknown, count: number, explicit: boolean, desc: string}} BulkTarget */

  /** @param {string|null} repo @returns {Promise<string[]>} */
  async function catalogFieldNames(repo) {
    if (!repo) return [];
    return [...(await catalog.read(repo)).keys()].sort();
  }

  /** The value type recorded for `field` in the repo catalog, else `string`.
   *  @param {string} repo @param {string} field */
  async function catalogType(repo, field) {
    return (await catalog.read(repo)).get(field) ?? 'string';
  }

  /** The value views for a bulk value arg (repo-wide type lookup); mirrors the
   *  direct-command views, `[ref-seeds]` rules included.
   *  @param {string} field @param {string|null} type */
  async function bulkValueViews(field, type) {
    const repo = await activeRepo();
    if (!repo) return [];
    return vocab.views(repo, field, type ?? (await catalogType(repo, field)));
  }

  // The repo the bulk arguments were last warmed against. The type argument's
  // `when` is synchronous and `activeRepo()` is not, so the field argument's
  // prompt — which is awaited, and always comes first — leaves it here.
  /** @type {string|null} */
  let bulkArgRepo = null;

  /** The type a bulk write on `field` would use without asking: the repo's
   *  catalogue alone. The displayed metarecord says nothing here — a bulk
   *  operation writes to *other* records. @param {string} field */
  function settledBulkType(field) {
    const repo = bulkArgRepo ?? shownRepo();
    return settledType({ name: field, catalog: repo ? catalog.known(repo, field) : null });
  }

  /** The type of a bulk write whose invocation carried none — the inline case.
   *  Refuses rather than guessing `string` (doc "Editing metarecords in the GUI").
   *  @param {string} repo @param {string} field */
  async function requireBulkType(repo, field) {
    await warmCatalog(repo);
    const known = catalog.known(repo, field);
    if (typeof known === 'string' && known !== 'nothing') return known;
    throw new Error(
      `no type known for "${field}" — name one: metarecord:bulk <op> <field> <type> <value>`,
    );
  }

  /** Counts the metarecords a query matches (daemon-side COUNT, no page load).
   *  @param {string} repo @param {unknown} query */
  async function countMatches(repo, query) {
    const result = /** @type {{total?: number|null}} */ (
      await daemon.call('POST', `/repos/${repo}/query`, { query, select: '*', limit: 1, count: true })
    );
    return result.total ?? 0;
  }

  /** The query the list actually shows, with its match count — the `query`
   *  target of a bulk command. `all` is the MATCH_ALL tautology, so prompts and
   *  confirmations can say "every metarecord" rather than a bare count.
   *  Read through `mf`: the command's run, or the panel's API for a prompt.
   *  @param {MetafolderApi} mf @param {string} repo
   *  @returns {Promise<{query: unknown, count: number, all: boolean}>} */
  async function bulkQueryTarget(mf, repo) {
    // The query the list actually shows (finder narrowing included), else — if
    // the list has not published one yet — its base query text.
    const effective = await mf.workspace.get('metarecord-list:effective-query');
    let query;
    let all;
    if (effective && typeof effective === 'object') {
      // MATCH_ALL is the "empty query matches all" tautology (deep-equal check).
      all = JSON.stringify(effective) === JSON.stringify(MATCH_ALL);
      query = effective;
    } else {
      // The list's base query, as normal DSL. NOT `metarecord-list:query`:
      // that one holds the *simplified* text, and parsing it with the normal
      // DSL parser either threw or — worse — read `#jazz` as something else
      // entirely, so a bulk edit could target a set the user never asked for.
      const raw = await mf.workspace.get('metarecord-list:base-query-text');
      const dsl = typeof raw === 'string' ? raw.trim() : '';
      all = dsl === '';
      query = dsl === '' ? MATCH_ALL : await metafolder.query.parse(dsl);
    }
    const count = await countMatches(repo, query);
    return { query, count, all };
  }

  /** Resolves what a bulk command targets, by its argument: `selection` acts
   *  on the checked records, `query` on what the list shows. `count` is always
   *  the number of metarecords the target holds (the selection size, or a
   *  COUNT of the query), so confirmations can name it. `explicit` tells the
   *  checkbox selection from the query — and an empty selection is nothing to
   *  do, never a detour onto the query. Read through `mf`, the command's run.
   *  @param {MetafolderApi} mf @param {string} target
   *  @returns {Promise<BulkTarget>} */
  async function bulkTarget(mf, target) {
    const repo = await activeRepo();
    if (!repo) throw new Error('no active repository');
    if (target === 'selection') {
      const selected = /** @type {string[]} */ (
        (await mf.workspace.get('selected_metarecords')) ?? []
      );
      return {
        repo,
        query: { type: 'uuid_in', uuids: selected },
        count: selected.length,
        explicit: true,
        desc: `${selected.length} selected metarecord${selected.length === 1 ? '' : 's'}`,
      };
    }
    const { query, count, all } = await bulkQueryTarget(mf, repo);
    return { repo, query, count, explicit: false, desc: all ? 'ALL metarecords' : 'the current query' };
  }

  /** The target argument's prompt names what each choice would act on — the
   *  checked count and the query's match count — so the answer is read with
   *  its consequence. Falls back to a plain prompt when the counts cannot be
   *  read (no active repository, or the daemon does not answer).
   *  @returns {Promise<string>} */
  async function bulkTargetPrompt() {
    try {
      const repo = await activeRepo();
      if (!repo) return 'Target? (selection / query)';
      const selected = /** @type {string[]} */ ((await workspace.get('selected_metarecords')) ?? []);
      const checked = selected.length;
      const { count, all } = await bulkQueryTarget(metafolder, repo);
      const matches = all ? `ALL metarecords (${count})` : `${count} matching`;
      return `Target? (selection = ${checked} checked · query = ${matches})`;
    } catch {
      return 'Target? (selection / query)';
    }
  }

  /** A bulk write is confirmed first, whatever its target, naming what it will
   *  change: the checked metarecords by their count, the query by its match
   *  count. An empty target is nothing to do — an empty selection is *not* a
   *  detour onto the query — and says so. Returns false when there is nothing
   *  to act on, or when the user declined. @param {BulkTarget} t @param {string} action */
  async function confirmBulk(t, action) {
    if (t.count === 0) {
      void statusBar.message(
        t.explicit ? 'No metarecords are checked — nothing to do.' : 'No metarecords match — nothing to do.',
        statusMessageMs,
      );
      return false;
    }
    const on = t.explicit ? t.desc : `${t.count} metarecord${t.count === 1 ? '' : 's'}`;
    return confirm(`${action} on ${on}?`);
  }

  // One command for the whole family, the target and the operation as its
  // first two arguments — the set-layer mirror of `metarecord:field`, over the
  // target the invocation names (the selection or the current query) instead of
  // the shown metarecord. Same vocabulary as the daemon routes it calls
  // (`query/fields/{set,add,remove,unset}`) and as `mf metarecord field`;
  // `delete` is the one that destroys the metarecords themselves
  // (`query/delete`), which is why it names no field.

  /**
   * @typedef {object} BulkOp
   * @property {string} [fieldPrompt] the second argument (absent for `delete`)
   * @property {boolean} [typed] whether the operation writes a value, and so
   *   needs a type — asked as an argument when the catalogue settles none
   * @property {(prior: string[]) => string} [valuePrompt]
   *   the third argument (absent for `unset` and `delete`)
   * @property {(field: string) => string} [confirm] what the confirmation says
   * @property {(mf: MetafolderApi, target: BulkTarget, field: string, raw: string,
   *             type: string|null) => Promise<void>} run `mf`: the command's run
   */

  /** Bulk operations, keyed by the second argument of `metarecord:bulk`.
   *  Tolerant of an unknown operation for the same reason as FIELD_OPS: the
   *  spec functions run before the dispatcher's error boundary.
   *  @type {Record<string, BulkOp>} */
  const BULK_OPS = {
    set: {
      typed: true,
      fieldPrompt: 'Field to set?',
      valuePrompt: (p) => `Value for "${p[2]}"?`,
      confirm: (field) => `Set "${field}"`,
      run: async (mf, t, field, raw, type) => {
        const value = await bulkValue(t, field, raw, type);
        const n = await bulkCall(mf, t, 'set', { name: field, value }, field);
        void statusBar.message(
          `"${field}" set on ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    add: {
      typed: true,
      fieldPrompt: 'Field to add a value to?',
      valuePrompt: (p) => `Value to add to "${p[2]}"?`,
      confirm: (field) => `Add a value to "${field}"`,
      run: async (mf, t, field, raw, type) => {
        const value = await bulkValue(t, field, raw, type);
        const n = await bulkCall(mf, t, 'add', { name: field, value }, field);
        void statusBar.message(
          `Value added to "${field}" on ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    remove: {
      typed: true,
      fieldPrompt: 'Field to remove a value from?',
      valuePrompt: (p) => `Value to remove from "${p[2]}"?`,
      confirm: (field) => `Remove a value from "${field}"`,
      run: async (mf, t, field, raw, type) => {
        const value = await bulkValue(t, field, raw, type);
        const n = await bulkCall(mf, t, 'remove', { name: field, value }, field);
        void statusBar.message(
          `Value removed from "${field}" on ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    unset: {
      fieldPrompt: 'Field to remove?',
      confirm: (field) => `Remove "${field}"`,
      run: async (mf, t, field) => {
        const n = await bulkCall(mf, t, 'unset', { name: field }, field);
        void statusBar.message(
          `"${field}" removed from ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    delete: {
      // No field, no value: this one destroys the metarecords. Undo brings
      // them back, but it touches a set, so it asks first (doc "Confirming a
      // trashing"), on its own terms rather than through confirmBulk: the
      // question says what goes and that the files stay.
      run: async (mf, t) => {
        if (t.count === 0) {
          void statusBar.message(
            t.explicit
              ? 'No metarecords are checked — nothing to delete.'
              : 'No metarecords match — nothing to delete.',
            statusMessageMs,
          );
          return;
        }
        if (
          !confirm(
            `Delete ${t.count} metarecord${t.count === 1 ? '' : 's'}? ` +
              `This removes the metarecords (any files stay on disk).`,
          )
        )
          return;
        const resp = /** @type {{deleted?: number}} */ (
          await daemon.call('POST', `/repos/${t.repo}/query/delete`, { query: t.query })
        );
        const deleted = resp.deleted ?? 0;
        void statusBar.message(
          `Deleted ${deleted} metarecord${deleted === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
        await dirty(mf);
      },
    },

    trash: {
      // The files of the whole set go to the trash, their metarecords with
      // them, in one write (doc "Sending files to the trash"). No field, no
      // value; confirmed on its own terms like `delete`, naming the count.
      // The Rust side posts the outcome (files trashed, metarecords without a
      // file kept) and marks the lists dirty.
      run: async (mf, t) => {
        if (t.count === 0) {
          void statusBar.message(
            t.explicit
              ? 'No metarecords are checked — nothing to trash.'
              : 'No metarecords match — nothing to trash.',
            statusMessageMs,
          );
          return;
        }
        const on = t.explicit ? t.desc : `${t.count} metarecord${t.count === 1 ? '' : 's'}`;
        if (
          !confirm(
            `Send the files of ${on} to the trash? ` +
              `Their metarecords go with them; restore them from the trash panel.`,
          )
        )
          return;
        await mf.trash.trashQuery(t.repo, t.query);
      },
    },
  };

  const BULK_OPERATIONS = Object.keys(BULK_OPS);

  /** Parses the raw text into a value of the type the invocation carries, or
   *  of the field's established one when it named none.
   *  @param {BulkTarget} t @param {string} field @param {string} raw
   *  @param {string|null} type */
  async function bulkValue(t, field, raw, type) {
    return vocab.parse(t.repo, field, type ?? (await requireBulkType(t.repo, field)), raw);
  }

  /** One set-layer call over the target, returning the number of metarecords
   *  the daemon reports changed (falling back to the target count).
   *  @param {MetafolderApi} mf @param {BulkTarget} t @param {string} route
   *  @param {object} body @param {string} field */
  async function bulkCall(mf, t, route, body, field) {
    const force = forceFor(field);
    const resp = /** @type {{updated?: number}} */ (
      await daemon.call('POST', `/repos/${t.repo}/query/fields/${route}`, {
        query: t.query,
        ...body,
        ...force,
      })
    );
    await dirty(mf);
    return resp.updated ?? t.count;
  }

  void commands.register('metarecord:bulk', {
    label: `Bulk operation on the selection or the current query (${BULK_OPERATIONS.join(' / ')})`,
    args: [
      {
        name: 'target',
        // What the operation acts on, named up front rather than inferred from
        // whether anything is checked: an inferred target is how a bulk write
        // meant for the checks landed on the whole query (or the reverse).
        // Pre-filled with the old implicit precedence, so the common answer is
        // still Enter — but visible and editable.
        prompt: () => bulkTargetPrompt(),
        initial: async () => {
          const selected = /** @type {string[]} */ (
            (await workspace.get('selected_metarecords')) ?? []
          );
          return selected.length > 0 ? 'selection' : 'query';
        },
        complete: () => ['selection', 'query'],
      },
      {
        name: 'operation',
        prompt: () => `Operation? (${BULK_OPERATIONS.join(' / ')})`,
        complete: () => BULK_OPERATIONS,
      },
      {
        name: 'field',
        when: (p) => BULK_OPS[p[1]]?.fieldPrompt !== undefined,
        // Awaited, so the repo and its catalogue are both resident by the time
        // the type argument below decides — synchronously — whether to ask.
        prompt: async (p) => {
          bulkArgRepo = await activeRepo();
          await warmCatalog(bulkArgRepo);
          return BULK_OPS[p[1]]?.fieldPrompt ?? 'Field?';
        },
        complete: async () => catalogFieldNames(await activeRepo()),
      },
      {
        name: 'type',
        // Same rule as `metarecord:field`: asked only when nothing settles it,
        // and before the value it types.
        when: (p) => BULK_OPS[p[1]]?.typed === true && settledBulkType(p[2]) === null,
        prompt: (p) => `Type for "${p[2]}"?`,
        complete: () => CONCRETE_TYPES,
      },
      {
        name: 'value',
        when: (p) => BULK_OPS[p[1]]?.valuePrompt !== undefined,
        prompt: (p) => BULK_OPS[p[1]]?.valuePrompt?.(p) ?? 'Value?',
        views: (p) => bulkValueViews(p[2], p.length > 3 ? p[3] : null),
      },
    ],
    handler: async (mf, target, op, ...rest) => {
      if (target !== 'selection' && target !== 'query') {
        throw new Error(
          `unknown target "${target}" — metarecord:bulk <selection|query> <op> [field] [type] [value]`,
        );
      }
      const spec = BULK_OPS[op];
      if (!spec) throw new Error(`unknown bulk operation: "${op}"`);
      const field = rest.shift() ?? '';
      const { type, value } = splitTypeValue(rest);
      const t = await bulkTarget(mf, target);
      if (spec.confirm && !(await confirmBulk(t, spec.confirm(field)))) return;
      await spec.run(mf, t, field, value, type);
    },
  });
}
