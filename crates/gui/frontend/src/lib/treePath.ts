// A path or a name as the daemon gives it (doc "Tree names"): a plain string
// while it is text, `{text, bytes}` when it is not. The GUI holds paths as the
// text it shows — its file handles are that text (`fs_path.rs`) — so it reads
// the object form as its text, never dropping it nor showing an object.

/** A path or name on the wire. */
export type WireText = string | { text: string; bytes: string };

/** The text a path or name shows, `undefined` for anything else. */
export function wireText(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  if (value && typeof value === "object") {
    const text = (value as { text?: unknown }).text;
    if (typeof text === "string") return text;
  }
  return undefined;
}

/** The texts of a list of paths, skipping what is neither form. */
export function wireTexts(values: unknown): string[] {
  if (!Array.isArray(values)) return [];
  return values.map(wireText).filter((t): t is string => t !== undefined);
}
