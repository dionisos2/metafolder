// The detail panel's value vocabulary: how a value reads back as one line of
// text, how that text parses back into a value, and what a value argument
// completes over — the same for the commands on the displayed metarecord and
// for the bulk ones. Plus the field catalogue (name → type) the type
// decisions consult. Everything here takes its repository as an argument: it
// knows nothing of which metarecord the panel shows.

import { parseRawValue, rawValueCompletions, TYPES } from '/__value-widget.js';
import { createRefSeeds } from './ref-seeds.js';

/** The one-line form of an explicit absence in a value slot — the glyph
 *  `formatValue` renders it as. It names the Nothing rows, which carry no
 *  raw form to parse. */
export const NOTHING = '∅';

/** The concrete value types a type argument completes over — every type but
 *  `nothing`, which is an absence and carries no value to parse. */
export const CONCRETE_TYPES = TYPES.filter((t) => t !== 'nothing');

/** A value's payload in an order-independent shape, so identity comparison
 *  below cannot read a key order as a difference.
 *  @param {Metafolder.Value} v */
function payloadOf(v) {
  switch (v.type) {
    case 'nothing':
      return null;
    case 'tree_ref':
    case 'hierarchy_ref':
      return [v.value.parent, v.value.name];
    case 'externalref':
      return [v.value.repo, v.value.metarecord];
    default:
      return v.value;
  }
}

/** Value identity, for the removal match: same type, same payload.
 *  @param {Metafolder.Value} a @param {Metafolder.Value} b */
export function sameValue(a, b) {
  return a.type === b.type && JSON.stringify(payloadOf(a)) === JSON.stringify(payloadOf(b));
}

/** `values` without its repeats — a field holds a value once (doc "No
 *  duplicate rows"). @param {Metafolder.Value[]} values */
export function distinctValues(values) {
  /** @type {Metafolder.Value[]} */
  const out = [];
  for (const v of values) if (!out.some((o) => sameValue(o, v))) out.push(v);
  return out;
}

/** The one view of a plain candidate list. @param {string} title
 *  @param {Promise<string[]>} items */
function plainView(title, items) {
  return { title, items: async () => ({ items: await items }) };
}

/**
 * The repository's field catalogue (name → type) as last read — what the
 * synchronous decisions (a type argument's `when`) consult. Every path that
 * leads to one of them reads it first, so it is never older than the
 * gesture; nothing else keeps it.
 * @param {MetafolderApi['daemon']} daemon
 */
export function createFieldCatalog(daemon) {
  /** @type {{repo: string|null, types: Map<string, string>}} */
  let catalog = { repo: null, types: new Map() };
  return {
    /** Reads `repo`'s field catalogue from the daemon (one cheap read). A
     *  daemon that does not answer leaves the last one read.
     *  @param {string} repo @returns {Promise<Map<string, string>>} */
    async read(repo) {
      try {
        const types = new Map((await daemon.fields(repo)).map((f) => [f.name, f.type]));
        catalog = { repo, types };
      } catch {
        if (catalog.repo !== repo) catalog = { repo, types: new Map() };
      }
      return catalog.types;
    },
    /** The catalogue's type for `name`, as last read for `repo` (no call).
     *  @param {string} repo @param {string} name */
    known(repo, name) {
      return catalog.repo === repo ? (catalog.types.get(name) ?? null) : null;
    },
  };
}

/**
 * @param {{
 *   daemon: MetafolderApi['daemon'],
 *   config: MetafolderApi['config'],
 *   parseQuery: (dsl: string) => Promise<unknown>,
 * }} ctx
 */
export function createValues({ daemon, config, parseQuery }) {
  /** The `[ref-seeds]` engine of a field (doc "Ref value seeds"), built
   *  once per field — its columns parse once, its pages come per call — and
   *  null for a field no rule names (not even the `*` default), where the
   *  legacy completion seeds still speak.
   *  @type {Map<string, Promise<ReturnType<typeof createRefSeeds>|null>>} */
  const refSeedsCache = new Map();
  /** @param {string} repo @param {string} field */
  function refSeedsFor(repo, field) {
    const key = `${repo}|${field}`;
    let hit = refSeedsCache.get(key);
    if (!hit) {
      hit = buildRefSeeds(repo, field);
      // A failed build must not be cached (like the tree paths): a config
      // fixed while the panel is open must take effect on the next prompt.
      hit.catch(() => refSeedsCache.delete(key));
      refSeedsCache.set(key, hit);
    }
    return hit;
  }

  /** @param {string} repo @param {string} field */
  async function buildRefSeeds(repo, field) {
    const rule = await config.refSeed(field);
    if (!rule) return null;
    const separator = await config.labelSeparator();
    return createRefSeeds({
      rule,
      separator,
      parseQuery,
      runQuery: async (query, opts) => {
        const body = /** @type {{results?: Metafolder.Metarecord[], total?: number}} */ (
          await daemon.call('POST', `/repos/${repo}/query`, {
            query,
            select: '*',
            sort: opts.sort,
            limit: opts.limit,
            count: true,
          })
        );
        return { records: body.results ?? [], total: body.total ?? null };
      },
      resolvePaths: (f, uuids) => daemon.treePaths(repo, f, uuids),
      getMetarecords: async (uuids) => Object.fromEntries(await daemon.metarecords(repo, uuids)),
    });
  }

  /** A one-line editable form of a value — the inverse of `parse`.
   *  Async: a tree_ref renders as its resolved path, and a `ref` with a
   *  completion seed (doc "Ref value seeds") as its path in the seed
   *  forest. Values are *entered* that way, so every form that shows one to be
   *  edited or removed shows it that way too.
   *  @param {string} repo @param {string} uuid @param {string} field
   *  @param {Metafolder.Value} value */
  async function rawOf(repo, uuid, field, value) {
    switch (value.type) {
      case 'nothing':
        return '';
      case 'bool':
        return value.value ? 'true' : 'false';
      case 'ref': {
        // A `[ref-seeds]` rule names the target (doc "Ref value seeds"):
        // the value reads back as the label its naming gives the target — what
        // is typed is what is shown.
        const seeds = await refSeedsFor(repo, field);
        if (seeds) {
          const label = await seeds.labelOf(String(value.value)).catch(() => null);
          if (label) return label;
        }
        // A target the naming gives no label keeps its uuid — unambiguous
        // either way, only less legible.
        return String(value.value);
      }
      case 'refbase':
        return String(value.value);
      case 'tree_ref': {
        const body = /** @type {{paths?: string[]}} */ (
          await daemon.call(
            'GET',
            `/repos/${repo}/metarecords/${uuid}/fields/${encodeURIComponent(field)}/resolve-tree`,
          )
        );
        return body.paths?.[0] ?? value.value.name;
      }
      case 'hierarchy_ref':
        // One row per parent (doc "Hierarchy references"): the row reads as
        // the path through its own parent, which the node's first path may
        // not go through.
        return pathThrough(repo, field, value.value.parent, value.value.name);
      case 'externalref':
        // Same one-line form as formatValue (ui.js); the value is an object, so
        // the default String(...) would render "[object Object]".
        return `${value.value.repo} :: ${value.value.metarecord}`;
      default:
        // Only primitive-valued types (string/int/float/datetime) reach here.
        return String(value.value);
    }
  }

  /** The path a position named `name` under `parent` reads as in `field` —
   *  what `parse` takes back to that parent and name. A root is its bare name.
   *  @param {string} repo @param {string} field @param {string|null} parent
   *  @param {string} name @returns {Promise<string>} */
  async function pathThrough(repo, field, parent, name) {
    if (!parent) return name;
    const above = await nodePath(repo, field, parent);
    return above === null ? name : above === '' ? `/${name}` : `${above}/${name}`;
  }

  /** A node's path in `field` (its first, for a node of several parents),
   *  null when it has none. @param {string} repo @param {string} field
   *  @param {string} uuid @returns {Promise<string|null>} */
  async function nodePath(repo, field, uuid) {
    const body = /** @type {{paths?: string[]}} */ (
      await daemon.call(
        'GET',
        `/repos/${repo}/metarecords/${uuid}/fields/${encodeURIComponent(field)}/resolve-tree`,
      )
    );
    return body.paths?.[0] ?? null;
  }

  /** A hierarchy row from its parent alone (doc "Hierarchy references"): a
   *  node has one name, so a new parent is all a further row needs. `raw` is
   *  the parent's path; empty, or `/`, places the node at the root.
   *  @param {string} repo @param {string} field @param {string} raw
   *  @param {string} name @returns {Promise<Metafolder.Value>} */
  async function parseParent(repo, field, raw, name) {
    const path = raw.trim();
    if (path === '' || path === '/') {
      return { type: 'hierarchy_ref', value: { parent: null, name } };
    }
    const res = /** @type {{uuid: string|null}} */ (
      await daemon.call('POST', `/repos/${repo}/tree/resolve-path`, { field, path })
    );
    if (!res.uuid) throw new Error(`parent path "${path}" does not exist in "${field}"`);
    return { type: 'hierarchy_ref', value: { parent: res.uuid, name } };
  }

  /** What `parseParent` takes back to `parent`: its path, empty for a root.
   *  @param {string} repo @param {string} field @param {string|null} parent */
  async function parentRaw(repo, field, parent) {
    return parent ? ((await nodePath(repo, field, parent)) ?? parent) : '';
  }

  /** What a `ref` value reads back as under the value: its label in the
   *  field's `[ref-seeds]` naming (doc "Ref value seeds"), else null — the
   *  target's `name` then applies.
   *  @param {string} repo @param {string} field @param {string} uuid */
  async function refLabel(repo, field, uuid) {
    const seeds = await refSeedsFor(repo, field);
    return seeds ? seeds.labelOf(uuid).catch(() => null) : null;
  }

  /** Builds a Value of `type` from a one-line raw string. tree_ref is special:
   *  `raw` is a PATH, whose parent is resolved to a uuid via /tree/resolve-path.
   *  ref is special when the field has a completion seed (doc "Ref value seeds"):
   *  `raw` is a PATH in the seed tree_ref field, resolved to the
   *  target uuid (a 32-hex `raw` is always taken as the uuid directly).
   *  @param {string} repo @param {string} field @param {string} type @param {string} raw */
  async function parse(repo, field, type, raw) {
    if (type === 'ref') {
      // A `[ref-seeds]` rule names the targets (doc "Ref value seeds"):
      // what is typed is a label of its naming, whole, resolved to the uuid it
      // names (an explicit uuid always wins).
      const seeds = await refSeedsFor(repo, field);
      if (seeds) return { type, value: await seeds.resolve(raw) };
      // No rule names this field (not even `*`): the value is the uuid typed.
      return { type, value: raw.trim() };
    }
    if (type !== 'tree_ref' && type !== 'hierarchy_ref') return parseRawValue(type, raw);
    const path = raw.trim();
    const slash = path.lastIndexOf('/');
    if (slash === -1) return { type, value: { parent: null, name: path } };
    const parentPath = path.slice(0, slash);
    const name = path.slice(slash + 1);
    const res = /** @type {{uuid: string|null}} */ (
      await daemon.call('POST', `/repos/${repo}/tree/resolve-path`, { field, path: parentPath })
    );
    if (!res.uuid) throw new Error(`parent path "${parentPath}" does not exist in "${field}"`);
    return { type, value: { parent: res.uuid, name } };
  }

  /** Memoized forest paths, keyed `repo|field`. Building one is a whole-forest
   *  `resolve-tree` plus tens of thousands of strings (~1 s on a 50 000-record
   *  repository); `m s` / `m a` / `m v` would each pay it again. The promise is
   *  cached, so concurrent prompts share one round-trip. Dropped whenever the
   *  data changes — a stale candidate list would offer paths that no longer
   *  exist. @type {Map<string, Promise<string[]>>} */
  const treePathsCache = new Map();

  /** Every path in `field`'s forest, for tree_ref value completion (a static
   *  list filtered client-side; bounded by the forest size). @param {string} repo @param {string} field */
  function treePathsForField(repo, field) {
    const key = `${repo}|${field}`;
    const hit = treePathsCache.get(key);
    if (hit) return hit;
    const pending = (async () => {
      const body = /** @type {Record<string, string[]>} */ (
        await daemon.call('POST', `/repos/${repo}/query/fields/resolve-tree`, {
          query: { type: 'is_present', field },
          field,
        })
      );
      const paths = new Set();
      for (const list of Object.values(body)) for (const p of list) paths.add(p);
      return [...paths].sort();
    })();
    // A failed build must not be cached, or the field completes to nothing
    // until the panel is remounted.
    pending.catch(() => treePathsCache.delete(key));
    treePathsCache.set(key, pending);
    return pending;
  }

  /** The views of a value argument for `type` on `field` (doc "Completion views"):
   *  the field's `[ref-seeds]` naming when it has a rule — the whole
   *  line, then each column, `completion:cycle` walking them — and one plain
   *  view otherwise (a tree_ref's own forest, a legacy seed forest's paths, a
   *  closed value set). @param {string} repo @param {string} field
   *  @param {string} type */
  async function views(repo, field, type) {
    if (type === 'ref') {
      const seeds = await refSeedsFor(repo, field);
      if (seeds) return seeds.views();
    }
    // A closed value set (bool: true/false) needs no repository lookup.
    const closed = rawValueCompletions(type);
    if (closed.length > 0) return [plainView(type, Promise.resolve(closed))];
    // A position completes over its own forest or hierarchy.
    return type === 'tree_ref' || type === 'hierarchy_ref'
      ? [plainView('path', treePathsForField(repo, field))]
      : [];
  }

  return {
    rawOf,
    pathThrough,
    parseParent,
    parentRaw,
    refLabel,
    parse,
    views,
    /** Drops the memoized forest paths (the data changed). */
    forgetTreePaths: () => treePathsCache.clear(),
  };
}
