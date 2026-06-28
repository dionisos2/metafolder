// Placeholder substitution for `!` shell commands (spec-gui "Command input").
// A `!` command line may reference the selected metarecord through `%`-tokens
// so the GUI can hand its identity to user scripts:
//
//   %u            → the selected metarecord's UUID
//   %v            → its version
//   %<field>      → the value of a single-valued scalar field
//   %<field>:path → a TreeRef field resolved to its absolute path
//   %%            → a literal percent
//
// Parsing and substitution are pure (and unit tested); the async orchestrator
// gathers the data through injected deps so it stays testable. On any
// unresolved token — no selection, an absent / multi-valued field, a bare
// TreeRef — expansion aborts and the command is NOT run (the caller surfaces
// the error), so a half-substituted shell line never executes.

/** A `%`-token: a field name plus whether the `:path` modifier was present. */
export interface Placeholder {
  name: string;
  path: boolean;
}

/** A command line split into literal text and placeholder tokens. */
export type Segment = { lit: string } | { ph: Placeholder };

export type ExpandOutcome = { ok: true; value: string } | { ok: false; error: string };

const TOKEN = /%(?:(%)|([A-Za-z_][A-Za-z0-9_]*)(:path)?)/g;

/** Split a shell command into literal/placeholder segments. A `%` that is not
 *  followed by `%` or a field name stays in the surrounding literal. */
export function parsePlaceholders(input: string): Segment[] {
  const segments: Segment[] = [];
  let last = 0;
  for (const m of input.matchAll(TOKEN)) {
    const at = m.index ?? 0;
    if (at > last) segments.push({ lit: input.slice(last, at) });
    if (m[1] === '%') segments.push({ lit: '%' });
    else segments.push({ ph: { name: m[2], path: m[3] !== undefined } });
    last = at + m[0].length;
  }
  if (last < input.length) segments.push({ lit: input.slice(last) });
  return segments;
}

/** The map key under which a placeholder's resolved value is stored. */
export function placeholderKey(ph: Placeholder): string {
  return ph.path ? `${ph.name}:path` : ph.name;
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
}

/** Format a single scalar field value, or report why it can't be used. */
function formatScalar(name: string, value: { type: string; value?: unknown }): ExpandOutcome {
  switch (value.type) {
    case 'string':
    case 'int':
    case 'float':
    case 'datetime':
      return { ok: true, value: String(value.value) };
    case 'bool':
      return { ok: true, value: value.value ? 'true' : 'false' };
    case 'nothing':
      return { ok: false, error: `field '${name}' is empty (Nothing)` };
    case 'tree_ref':
      return { ok: false, error: `field '${name}' is a tree reference; use %${name}:path` };
    default:
      return { ok: false, error: `field '${name}' has unsupported type '${value.type}'` };
  }
}

/** Expand the `%`-tokens in a `!` shell command. Returns the rewritten line, or
 *  an error (the command must then not be run). */
export async function expandShellPlaceholders(input: string, deps: ExpandDeps): Promise<ExpandOutcome> {
  const segments = parsePlaceholders(input);
  const placeholders = segments.filter((s): s is { ph: Placeholder } => 'ph' in s).map((s) => s.ph);
  if (placeholders.length === 0) return { ok: true, value: input };

  const sel = await deps.selected();
  if (!sel) return { ok: false, error: 'no metarecord selected' };

  // Fetch the metarecord once if any token needs its version or a field value
  // (everything except `%u`, which comes straight from the selection, and the
  // `:path` tokens, which use the tree-resolve endpoint).
  const needsRecord = placeholders.some((p) => !p.path && p.name !== 'u');
  let record: FetchedMetarecord | null = null;
  try {
    if (needsRecord) record = await deps.metarecord(sel.repo, sel.uuid);
  } catch (error) {
    return { ok: false, error: `cannot read selected metarecord: ${String(error)}` };
  }

  const resolved = new Map<string, string>();
  for (const ph of placeholders) {
    const key = placeholderKey(ph);
    if (resolved.has(key)) continue;

    let outcome: ExpandOutcome;
    if (ph.path) {
      try {
        const paths = await deps.treePaths(sel.repo, sel.uuid, ph.name);
        if (paths.length === 0) outcome = { ok: false, error: `field '${ph.name}' has no resolved path` };
        else if (paths.length > 1) outcome = { ok: false, error: `field '${ph.name}' resolves to multiple paths` };
        else outcome = { ok: true, value: paths[0] };
      } catch (error) {
        outcome = { ok: false, error: `cannot resolve '${ph.name}': ${String(error)}` };
      }
    } else if (ph.name === 'u') {
      outcome = { ok: true, value: sel.uuid };
    } else if (ph.name === 'v') {
      const version = record?.version;
      outcome =
        version === undefined
          ? { ok: false, error: 'metarecord has no version' }
          : { ok: true, value: String(version) };
    } else {
      const rows = (record?.fields ?? []).filter((f) => f.name === ph.name);
      if (rows.length === 0) outcome = { ok: false, error: `field '${ph.name}' is absent` };
      else if (rows.length > 1) outcome = { ok: false, error: `field '${ph.name}' is multi-valued` };
      else outcome = formatScalar(ph.name, rows[0].value);
    }

    if (!outcome.ok) return outcome;
    resolved.set(key, outcome.value);
  }

  return substitute(segments, resolved);
}
