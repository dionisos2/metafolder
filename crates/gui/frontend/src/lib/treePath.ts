// A path or a name as the daemon gives it (doc "Tree names"): a plain string
// while it is text, `{text, bytes}` when it is not. The GUI holds paths as the
// text where it only shows a path (`wireText`), and keeps the exact form where
// it acts on one (`wirePaths`) — its file handles are that form (`fs_path.rs`).

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

/** Whether `value` is a path or a name in either form. */
export function isWirePath(value: unknown): value is WireText {
  if (typeof value === "string") return true;
  return (
    !!value &&
    typeof value === "object" &&
    typeof (value as { text?: unknown }).text === "string" &&
    typeof (value as { bytes?: unknown }).bytes === "string"
  );
}

/** The paths of a list, exactly as given — what a client acts on — skipping
 *  what is neither form. */
export function wirePaths(values: unknown): WireText[] {
  return Array.isArray(values) ? values.filter(isWirePath) : [];
}

/** The texts of a list of paths, skipping what is neither form. */
export function wireTexts(values: unknown): string[] {
  if (!Array.isArray(values)) return [];
  return values.map(wireText).filter((t): t is string => t !== undefined);
}
