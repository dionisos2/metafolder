// Placeholder substitution for `!` shell commands (spec-gui "Command input").
// A `!` command line may reference the selection and the active workspace
// through `%`-tokens, so the GUI can hand their identity to user scripts:
//
//   %u            → the selected metarecord's UUID
//   %v            → its version
//   %p            → the selected absolute path (workspace.selected_paths)
//   %r            → the active repository's UUID (also %r:uuid)
//   %r:name       → the active repository's name
//   %<field>      → the value of a single-valued scalar field
//   %<field>:path → a TreeRef field resolved to its absolute path
//   %%            → a literal percent
//
// Parsing and substitution are pure (and unit tested); the async orchestrator
// gathers the data through injected deps so it stays testable. On any
// unresolved token — nothing selected, no active repo, an absent / multi-valued
// field, a bare TreeRef — expansion aborts and the command is NOT run (the
// caller surfaces the error), so a half-substituted shell line never executes.

/** A `%`-token: a name plus an optional `:modifier` (e.g. `path`, `name`). */
export interface Placeholder {
  name: string;
  mod: string | null;
}

/** A command line split into literal text and placeholder tokens. */
export type Segment = { lit: string } | { ph: Placeholder };

export type ExpandOutcome = { ok: true; value: string } | { ok: false; error: string };

const TOKEN = /%(?:(%)|([A-Za-z_][A-Za-z0-9_]*)(?::([a-z]+))?)/g;

/** Split a shell command into literal/placeholder segments. A `%` that is not
 *  followed by `%` or a name stays in the surrounding literal. */
export function parsePlaceholders(input: string): Segment[] {
  const segments: Segment[] = [];
  let last = 0;
  for (const m of input.matchAll(TOKEN)) {
    const at = m.index ?? 0;
    if (at > last) segments.push({ lit: input.slice(last, at) });
    if (m[1] === '%') segments.push({ lit: '%' });
    else segments.push({ ph: { name: m[2], mod: m[3] ?? null } });
    last = at + m[0].length;
  }
  if (last < input.length) segments.push({ lit: input.slice(last) });
  return segments;
}

/** The map key under which a placeholder's resolved value is stored. */
export function placeholderKey(ph: Placeholder): string {
  return ph.mod ? `${ph.name}:${ph.mod}` : ph.name;
}

/** POSIX single-quote: wrap in '…' and escape embedded quotes as '\''. */
function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

/** Rebuild the command line, shell-quoting each substituted value. Fails if a
 *  placeholder has no entry in `resolved` (a defensive guard — the orchestrator
 *  resolves every token first). */
export function substitute(segments: Segment[], resolved: Map<string, string>): ExpandOutcome {
  let out = '';
  for (const seg of segments) {
    if ('lit' in seg) {
      out += seg.lit;
      continue;
    }
    const key = placeholderKey(seg.ph);
    const value = resolved.get(key);
    if (value === undefined) return { ok: false, error: `unresolved placeholder: %${key}` };
    out += shellQuote(value);
  }
  return { ok: true, value: out };
}

// ── Async orchestration ────────────────────────────────────────────────

/** The shape of a metarecord field as returned by the daemon. */
interface FieldRow {
  name: string;
  value: { type: string; value?: unknown };
}

interface FetchedMetarecord {
  version?: number;
  fields?: FieldRow[];
}

/** Data sources the orchestrator needs, injected so it can be unit tested. */
export interface ExpandDeps {
  /** The current `selected_metarecord` workspace var, or null. */
  selected(): Promise<{ uuid: string; repo: string } | null>;
  /** Fetch the full metarecord (version + fields). */
  metarecord(repo: string, uuid: string): Promise<FetchedMetarecord>;
  /** Resolve a TreeRef field to its absolute paths. */
  treePaths(repo: string, uuid: string, field: string): Promise<string[]>;
  /** The current `selected_paths` workspace var (absolute paths). */
  selectedPaths(): Promise<string[]>;
  /** The active repository's UUID (`active_repo` workspace var), or null. */
  activeRepo(): Promise<string | null>;
  /** The display name of a repository, given its UUID. */
  repoName(repo: string): Promise<string>;
}

/** A token that cannot be resolved; its message is shown to the user. */
class ResolveError extends Error {}

/** Memoize a zero-arg async thunk (its result, or its rejection). */
function once<T>(fn: () => Promise<T>): () => Promise<T> {
  let pending: Promise<T> | undefined;
  return () => (pending ??= fn());
}

/** Format a single scalar field value, or throw why it can't be used. */
function formatScalar(name: string, value: { type: string; value?: unknown }): string {
  switch (value.type) {
    case 'string':
    case 'int':
    case 'float':
    case 'datetime':
      return String(value.value);
    case 'bool':
      return value.value ? 'true' : 'false';
    case 'nothing':
      throw new ResolveError(`field '${name}' is empty (Nothing)`);
    case 'tree_ref':
      throw new ResolveError(`field '${name}' is a tree reference; use %${name}:path`);
    default:
      throw new ResolveError(`field '${name}' has unsupported type '${value.type}'`);
  }
}

/** The single value of a path list, or throw if there are none or several. */
function singlePath(paths: string[], what: string): string {
  if (paths.length === 0) throw new ResolveError(`${what} has no value`);
  if (paths.length > 1) throw new ResolveError(`${what} has multiple values`);
  return paths[0];
}

/** Expand the `%`-tokens in a `!` shell command. Returns the rewritten line, or
 *  an error (the command must then not be run). */
export async function expandShellPlaceholders(input: string, deps: ExpandDeps): Promise<ExpandOutcome> {
  const segments = parsePlaceholders(input);
  const placeholders = segments.filter((s): s is { ph: Placeholder } => 'ph' in s).map((s) => s.ph);
  if (placeholders.length === 0) return { ok: true, value: input };

  // Lazily loaded, memoized data sources — only fetched if a token needs them.
  const sel = once(async () => {
    const s = await deps.selected();
    if (!s) throw new ResolveError('no metarecord selected');
    return s;
  });
  const record = once(async () => {
    const s = await sel();
    return deps.metarecord(s.repo, s.uuid);
  });
  const repo = once(async () => {
    const r = await deps.activeRepo();
    if (!r) throw new ResolveError('no active repository');
    return r;
  });

  async function resolveOne(ph: Placeholder): Promise<string> {
    const { name, mod } = ph;
    // Reserved shorthands (not field names).
    if (name === 'u' || name === 'v' || name === 'p' || name === 'r') {
      if (name === 'r') {
        if (mod === null || mod === 'uuid') return repo();
        if (mod === 'name') return deps.repoName(await repo());
        throw new ResolveError(`unknown modifier ':${mod}' for %r`);
      }
      if (mod !== null) throw new ResolveError(`%${name} takes no modifier`);
      if (name === 'u') return (await sel()).uuid;
      if (name === 'p') return singlePath(await deps.selectedPaths(), 'the selected path');
      // name === 'v'
      const version = (await record()).version;
      if (version === undefined) throw new ResolveError('metarecord has no version');
      return String(version);
    }
    // A field name.
    if (mod === 'path') {
      const s = await sel();
      return singlePath(await deps.treePaths(s.repo, s.uuid, name), `field '${name}'`);
    }
    if (mod !== null) throw new ResolveError(`unknown modifier ':${mod}' for field '${name}'`);
    const rows = ((await record()).fields ?? []).filter((f) => f.name === name);
    if (rows.length === 0) throw new ResolveError(`field '${name}' is absent`);
    if (rows.length > 1) throw new ResolveError(`field '${name}' is multi-valued`);
    return formatScalar(name, rows[0].value);
  }

  const resolved = new Map<string, string>();
  try {
    for (const ph of placeholders) {
      const key = placeholderKey(ph);
      if (resolved.has(key)) continue;
      resolved.set(key, await resolveOne(ph));
    }
  } catch (error) {
    return { ok: false, error: error instanceof ResolveError ? error.message : String(error) };
  }

  return substitute(segments, resolved);
}
