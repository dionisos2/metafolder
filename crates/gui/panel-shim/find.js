// Find in panel (spec-gui "Find in panel"): a browser-style Ctrl-F over the
// *rendered* text of one panel's Shadow DOM. Framework-free and panel-agnostic
// — the shell drives it against whichever panel root is focused, so no panel
// has to implement anything.
//
// The flattening is what makes it usable on hand-written markup (the help
// pages): the source whitespace collapses the way the layout collapses it, so
// a term broken over two indented lines still matches, and block elements are
// separated so two paragraphs never concatenate into a word nobody can see.
// Only rendered text is searched, exactly like the browser's own find: a
// hidden subtree, a `<script>`, and rows a paged panel has not loaded are all
// invisible to it.

/** Elements whose text is never rendered. */
const SKIPPED = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE']);

/** Elements that break the text flow. Anything not listed counts as inline, so
 *  `hello <b>world</b>` stays one searchable run. */
const BLOCK = new Set([
  'ADDRESS', 'ARTICLE', 'ASIDE', 'BLOCKQUOTE', 'BR', 'DD', 'DETAILS', 'DIALOG', 'DIV', 'DL',
  'DT', 'FIELDSET', 'FIGCAPTION', 'FIGURE', 'FOOTER', 'FORM', 'H1', 'H2', 'H3', 'H4', 'H5',
  'H6', 'HEADER', 'HR', 'LI', 'MAIN', 'NAV', 'OL', 'P', 'PRE', 'SECTION', 'SUMMARY', 'TABLE',
  'TBODY', 'TD', 'TFOOT', 'TH', 'THEAD', 'TR', 'UL',
]);

/** The separator standing for a block boundary in the flattened text. A needle
 *  typed in a one-line input can never contain it, so a match never spans one. */
const BREAK = '\n';

/** Whether the element renders nothing. `hidden` and an inline `display:none`
 *  are checked directly (they cover the panels' own show/hide) and the computed
 *  style catches the rest (a class in the panel's stylesheet).
 *  @param {Element} element */
export function isHidden(element) {
  const styled = /** @type {HTMLElement} */ (element);
  if (styled.hidden === true) return true;
  const inline = styled.style;
  if (inline && (inline.display === 'none' || inline.visibility === 'hidden')) return true;
  const view = element.ownerDocument?.defaultView;
  if (!view?.getComputedStyle) return false;
  let style;
  try {
    style = view.getComputedStyle(element);
  } catch {
    return false; // a detached or exotic node: assume it renders
  }
  return style.display === 'none' || style.visibility === 'hidden';
}

/**
 * The flattened, searchable text of everything rendered under `root`, with the
 * map back to the DOM: `nodes` are the visited text nodes, and for each
 * character of `text`, `owner[i]` indexes into them (-1 for a block separator)
 * and `offset[i]` is its position inside that node.
 *
 * @param {Node} root a shadow root, document fragment or element
 * @returns {{text: string, nodes: Text[], owner: number[], offset: number[]}}
 */
export function flatten(root) {
  /** @type {Text[]} */
  const nodes = [];
  /** @type {string[]} */
  const chars = [];
  /** @type {number[]} */
  const owner = [];
  /** @type {number[]} */
  const offset = [];
  const last = () => (chars.length === 0 ? '' : chars[chars.length - 1]);

  function emitBreak() {
    if (chars.length === 0 || last() === BREAK) return;
    chars.push(BREAK);
    owner.push(-1);
    offset.push(0);
  }

  /** @param {Text} node */
  function emitText(node) {
    const data = node.data;
    if (data === '') return;
    let index = -1;
    for (let i = 0; i < data.length; i += 1) {
      const char = data[i];
      const space = char === ' ' || char === '\n' || char === '\t' || char === '\r' || char === '\f';
      if (space) {
        // Collapse runs, and drop whitespace that only follows a boundary.
        if (chars.length === 0 || last() === ' ' || last() === BREAK) continue;
        if (index < 0) index = nodes.push(node) - 1;
        chars.push(' ');
      } else {
        if (index < 0) index = nodes.push(node) - 1;
        chars.push(char);
      }
      owner.push(index);
      offset.push(i);
    }
  }

  /** @param {Node} node */
  function walk(node) {
    for (const child of node.childNodes) {
      if (child.nodeType === 3) {
        emitText(/** @type {Text} */ (child));
        continue;
      }
      if (child.nodeType !== 1) continue;
      const element = /** @type {Element} */ (child);
      if (SKIPPED.has(element.tagName) || isHidden(element)) continue;
      const block = BLOCK.has(element.tagName);
      if (block) emitBreak();
      walk(element.shadowRoot ?? element);
      if (block) emitBreak();
    }
  }

  walk(root);
  return { text: chars.join(''), nodes, owner, offset };
}

/**
 * Every non-overlapping, case-insensitive occurrence of `needle` in `text`, as
 * start offsets. An empty needle matches nothing (the find bar is simply idle).
 *
 * @param {string} text @param {string} needle @returns {number[]}
 */
export function matchAll(text, needle) {
  if (needle === '') return [];
  // Case folding must not change the length, or the offsets would not map back
  // (a handful of characters, e.g. 'İ', expand); those search case-sensitively.
  let haystack = text.toLowerCase();
  let term = needle.toLowerCase();
  if (haystack.length !== text.length || term.length !== needle.length) {
    haystack = text;
    term = needle;
  }
  const hits = [];
  for (let at = haystack.indexOf(term); at >= 0; at = haystack.indexOf(term, at + term.length)) {
    hits.push(at);
  }
  return hits;
}

/**
 * @typedef {{startNode: Text, startOffset: number, endNode: Text, endOffset: number}} Span
 *
 * Maps a `[start, end)` span of the flattened text back to its text nodes, or
 * null when the span covers no real character (only separators).
 *
 * @param {ReturnType<typeof flatten>} flat @param {number} start @param {number} end
 * @returns {Span|null}
 */
export function locate(flat, start, end) {
  if (end <= start) return null;
  const first = flat.owner[start];
  const lastOwner = flat.owner[end - 1];
  if (first < 0 || lastOwner < 0) return null;
  return {
    startNode: flat.nodes[first],
    startOffset: flat.offset[start],
    endNode: flat.nodes[lastOwner],
    endOffset: flat.offset[end - 1] + 1,
  };
}

/**
 * Every occurrence of `needle` in the rendered text under `root`, in document
 * order.
 *
 * @param {Node} root @param {string} needle @returns {Span[]}
 */
export function search(root, needle) {
  const flat = flatten(root);
  const spans = [];
  for (const at of matchAll(flat.text, needle)) {
    const span = locate(flat, at, at + needle.length);
    if (span) spans.push(span);
  }
  return spans;
}

/** Turns a span into a live DOM Range. @param {Span} span */
export function toRange(span) {
  const range = span.startNode.ownerDocument.createRange();
  range.setStart(span.startNode, span.startOffset);
  range.setEnd(span.endNode, span.endOffset);
  return range;
}

/** The two highlight registry names; the panel stylesheet styles them with
 *  `::highlight(mf-find)` / `::highlight(mf-find-current)`. */
export const HIGHLIGHT_ALL = 'mf-find';
export const HIGHLIGHT_CURRENT = 'mf-find-current';

/**
 * Paints the matches with the CSS Custom Highlight API — no DOM mutation, so a
 * panel's own markup and event handlers are never touched. A view without the
 * API simply gets no paint (the scroll-to-match still works).
 *
 * @param {typeof globalThis} [view]
 */
export function createHighlighter(view = globalThis) {
  const registry = view?.CSS?.highlights;
  const Ctor = view?.Highlight;
  return {
    supported: Boolean(registry && Ctor),
    /** @param {Range[]} ranges @param {number} current index into `ranges` */
    show(ranges, current) {
      if (!registry || !Ctor) return;
      const others = ranges.filter((_, i) => i !== current);
      registry.set(HIGHLIGHT_ALL, new Ctor(...others));
      const focused = ranges[current];
      registry.set(HIGHLIGHT_CURRENT, focused ? new Ctor(focused) : new Ctor());
    },
    clear() {
      if (!registry) return;
      registry.delete(HIGHLIGHT_ALL);
      registry.delete(HIGHLIGHT_CURRENT);
    },
  };
}

/** Margin, in pixels, inside which a match counts as too close to the edge to
 *  read comfortably. */
const EDGE = 24;

/**
 * Whether a match at `rect` needs the view scrolled, given the visible `box` it
 * sits in. A match already comfortably inside it is left alone: typing into the
 * find bar re-runs the search on every keystroke, and re-centring each time
 * would make the text jump under the reader. Pure, so it is unit tested.
 *
 * @param {{top: number, bottom: number}|null} rect
 * @param {{top: number, bottom: number}|null} box
 */
export function needsScroll(rect, box) {
  if (!rect || !box) return true;
  if (box.bottom - box.top <= 2 * EDGE) return true; // too small to keep a margin
  return rect.top < box.top + EDGE || rect.bottom > box.bottom - EDGE;
}

/** Scrolls a match into view when it is not comfortably visible already. Ranges
 *  have no `scrollIntoView`, so the nearest element does the scrolling; the
 *  visible box is the panel's own host (the match lives in its shadow tree),
 *  falling back to the window. @param {Range} range */
export function scrollRangeIntoView(range) {
  const node = range.startContainer;
  const element = /** @type {Element|null} */ (
    node.nodeType === 1 ? node : node.parentElement
  );
  if (!element?.scrollIntoView) return;
  const root = element.getRootNode?.();
  const host = root && 'host' in root ? /** @type {Element} */ (root.host) : null;
  const view = element.ownerDocument?.defaultView;
  const box = host
    ? host.getBoundingClientRect()
    : view
      ? { top: 0, bottom: view.innerHeight }
      : null;
  if (!needsScroll(range.getBoundingClientRect?.() ?? null, box)) return;
  element.scrollIntoView({ block: 'center', inline: 'nearest' });
}
