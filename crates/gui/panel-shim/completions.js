// The candidate rules every completion shares (doc "Completion views"):
// a candidate is a couple — the label shown (and matched whole) and the value
// the command receives — a plain string is both, and two candidates that read
// the same are both suffixed with their values so every listed row names
// exactly one thing. Shared by the shell's prompt driver (which publishes the
// pages) and by the panels that name and resolve values, because what is
// displayed whole is what must be typed whole.

/**
 * One candidate as the list holds it.
 * @typedef {{label: string, value: string}} Candidate
 */

/**
 * One page of candidates. `more` says the source holds more than it handed
 * over.
 * @typedef {{items: (string|Candidate)[], more?: boolean}} Page
 * @typedef {(string|Candidate)[] | Page} Pageish
 */

/**
 * One cycled view of a completion (doc "Completion views"): its title
 * while on screen, and its candidates for the typed text and the arguments
 * collected so far.
 * @typedef {{title?: string,
 *            items: (partial: string, prior: string[]) => Pageish | Promise<Pageish>}} View
 */

/**
 * Normalizes a builder's result to a page — a bare list is a complete one.
 * @param {{items: (string|Candidate)[], more?: boolean}|(string|Candidate)[]} result
 * @returns {{items: (string|Candidate)[], more?: boolean}}
 */
export function completionPage(result) {
  return Array.isArray(result) ? { items: result } : result;
}

/**
 * Normalizes candidates to label/value pairs and applies the *duplicate-label
 * rule*: two candidates that read the same are both suffixed with their
 * values, so every listed row is addressable and names exactly one thing. (In
 * a ref completion the value is a uuid — the suffix is what keeps two
 * same-named metarecords apart.) Exact duplicates (same label, same value)
 * collapse first.
 *
 * @param {(string|Candidate)[]} items
 * @returns {Candidate[]}
 */
export function completionLabels(items) {
  /** @type {Candidate[]} */
  const pairs = [];
  const had = new Set();
  for (const item of items) {
    const pair = typeof item === 'string' ? { label: item, value: item } : item;
    const key = `${pair.label}\u0000${pair.value}`;
    if (had.has(key)) continue;
    had.add(key);
    pairs.push(pair);
  }
  const byLabel = new Map();
  for (const pair of pairs) byLabel.set(pair.label, (byLabel.get(pair.label) ?? 0) + 1);
  return pairs.map((pair) =>
    byLabel.get(pair.label) > 1
      ? { label: `${pair.label} (${pair.value})`, value: pair.value }
      : pair,
  );
}
