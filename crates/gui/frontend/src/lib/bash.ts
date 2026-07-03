// Pure logic of the bash input's Tab completion (spec-gui "Command input").
// The candidates come from the Rust `bash_complete` command, which returns
// the word being completed (the text the candidates replace) alongside them.

export interface Insertion {
  text: string;
  cursor: number;
}

/** Longest prefix shared by every candidate (bash's partial completion). */
export function commonPrefix(items: string[]): string {
  if (items.length === 0) return '';
  let prefix = items[0];
  for (const item of items.slice(1)) {
    let length = 0;
    while (length < prefix.length && prefix[length] === item[length]) length += 1;
    prefix = prefix.slice(0, length);
    if (prefix === '') break;
  }
  return prefix;
}

/**
 * Replaces the completed word (which ends at `cursor`) with `candidate`.
 * A `final` (unique or accepted) completion gets a trailing space so typing
 * can continue — unless it is a directory (trailing `/`), matching bash;
 * a partial common-prefix insertion never does.
 */
export function insertCandidate(
  line: string,
  cursor: number,
  word: string,
  candidate: string,
  final: boolean,
): Insertion {
  const start = cursor - word.length;
  const inserted = candidate + (final && !candidate.endsWith('/') ? ' ' : '');
  return {
    text: line.slice(0, start) + inserted + line.slice(cursor),
    cursor: start + inserted.length,
  };
}
